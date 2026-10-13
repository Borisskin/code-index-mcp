use anyhow::{anyhow, Result};

use super::callee::is_plain_qualifier;
use super::types::MAX_VISIT_DEPTH;
use super::types::{
    empty_parse_result, looks_binary, sha256_hex, ParseResult, ParsedCall, ParsedFunction,
    ParsedVariable, PARSE_TIMEOUT_MS,
};
use super::LanguageParser;

/// Парсер BSL-файлов (1С:Предприятие / OneScript) на основе tree-sitter-bsl
pub struct BslParser;

impl Default for BslParser {
    fn default() -> Self {
        Self::new()
    }
}

impl BslParser {
    pub fn new() -> Self {
        BslParser
    }
}

impl LanguageParser for BslParser {
    fn language_name(&self) -> &str {
        "bsl"
    }

    fn file_extensions(&self) -> &[&str] {
        &["bsl", "os"]
    }

    fn parse(&self, source: &str, _file_path: &str) -> Result<ParseResult> {
        parse_bsl(source)
    }
}

/// Получить текст узла AST из байтового среза
fn node_text<'a>(node: tree_sitter::Node<'a>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Найти первый дочерний узел с заданным kind
fn find_child_by_kind<'a>(
    node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    let found = node
        .children(&mut cursor)
        .find(|&child| child.kind() == kind);
    found
}

/// Извлечь директиву компиляции и (для расширений) информацию о переопределении
/// метода. В грамматике `tree-sitter-bsl` директива компиляции НЕ является
/// дочерним узлом процедуры/функции — она лежит её ПРЕДЫДУЩИМ соседом в
/// `source_file`: `&НаСервере` даёт узел `preprocessor`, внутри которого —
/// `annotation` с текстом директивы (уже включающим `&`); `&Вместо("Foo")` —
/// тот же `preprocessor`, но рядом с `annotation` ещё и `string(string_content)`
/// с именем целевой процедуры. Возвращает (директива, тип_переопределения,
/// цель_переопределения).
fn extract_directive(
    node: tree_sitter::Node,
    source: &[u8],
) -> (Option<String>, Option<String>, Option<String>) {
    let prev = match node.prev_sibling() {
        Some(p) if p.kind() == "preprocessor" => p,
        _ => return (None, None, None),
    };

    let annotation = match find_child_by_kind(prev, "annotation") {
        Some(a) => a,
        None => return (None, None, None),
    };

    let directive = node_text(annotation, source).to_string();
    if directive.is_empty() {
        return (None, None, None);
    }

    // Имя директивы без & и в нижнем регистре — для сопоставления с
    // русским/английским названием переопределения расширения.
    let name_lower = directive.trim_start_matches('&').to_lowercase();
    let override_type = match name_lower.as_str() {
        "перед" | "before" => Some("Перед"),
        "после" | "after" => Some("После"),
        "вместо" | "around" => Some("Вместо"),
        "изменениеиконтроль" | "changeandvalidate" => Some("ИзменениеИКонтроль"),
        _ => None,
    };

    match override_type {
        Some(otype) => {
            // Цель переопределения — string_content внутри string, лежащего
            // рядом с annotation в том же preprocessor.
            let target = find_child_by_kind(prev, "string")
                .and_then(|s| find_child_by_kind(s, "string_content"))
                .map(|sc| node_text(sc, source).to_string());
            (Some(directive), Some(otype.to_string()), target)
        }
        None => (Some(directive), None, None),
    }
}

/// Контекст обхода AST BSL
struct VisitContext<'a> {
    source: &'a [u8],
    functions: Vec<ParsedFunction>,
    calls: Vec<ParsedCall>,
    variables: Vec<ParsedVariable>,
}

impl<'a> VisitContext<'a> {
    fn new(source: &'a [u8]) -> Self {
        VisitContext {
            source,
            functions: Vec::new(),
            calls: Vec::new(),
            variables: Vec::new(),
        }
    }
}

/// Рекурсивный обход узла AST BSL.
/// - `current_func` — имя ближайшей процедуры/функции-контейнера (для caller у вызовов)
fn visit_node(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    current_func: Option<&str>,
    depth: usize,
) {
    // Ограничение глубины для защиты от переполнения стека
    if depth > MAX_VISIT_DEPTH {
        return;
    }

    match node.kind() {
        "procedure_definition" => {
            visit_proc_or_func(node, ctx, "procedure");
        }
        "function_definition" => {
            visit_proc_or_func(node, ctx, "function");
        }
        "var_definition" => {
            visit_module_var(node, ctx);
        }
        "method_call" => {
            record_method_call(node, ctx, current_func);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, current_func, depth + 1);
            }
        }
        _ => {
            // Рекурсивно обходим остальные узлы
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, current_func, depth + 1);
            }
        }
    }
}

/// Обработать procedure_definition или function_definition
fn visit_proc_or_func(node: tree_sitter::Node, ctx: &mut VisitContext, proc_type: &str) {
    let source = ctx.source;

    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // Параметры из поля parameters
    let arg_list_text = node
        .child_by_field_name("parameters")
        .map(|n| node_text(n, source).to_string());

    // Экспорт: поле export (EXPORT_KEYWORD)
    let is_export = node.child_by_field_name("export").is_some();

    // Формируем строку аргументов: текст parameters + суффикс "Экспорт" если нужно
    let args = match (arg_list_text, is_export) {
        (Some(args_str), true) => Some(format!("{} Экспорт", args_str)),
        (Some(args_str), false) => Some(args_str),
        (None, true) => Some("() Экспорт".to_string()),
        (None, false) => None,
    };

    // Директива компиляции + переопределение расширения (annotation лежит в
    // ПРЕДЫДУЩЕМ соседе-preprocessor, не внутри самой процедуры/функции)
    let (directive, override_type, override_target) = extract_directive(node, source);

    // Метаинформация BSL в docstring: тип + директива + экспорт + переопределение
    let docstring = {
        let mut parts = vec![proc_type.to_string()];
        if let Some(ref d) = directive {
            parts.push(d.clone());
        }
        if is_export {
            parts.push("export".to_string());
        }
        if let (Some(ref ot), Some(ref otgt)) = (&override_type, &override_target) {
            parts.push(format!("override:{ot}({otgt})"));
        }
        Some(parts.join(" "))
    };

    // Полный текст (без предшествующей директивы — она лежит в соседнем узле)
    let body = node_text(node, source).to_string();
    let node_hash = sha256_hex(&body);

    let func_name = name.clone();

    ctx.functions.push(ParsedFunction {
        name,
        qualified_name: None, // В BSL нет вложенных классов
        line_start,
        line_end,
        args,
        // Директива компиляции — не тип результата. Она уже лежит в docstring
        // рядом с видом процедуры, признаком экспорта и переопределением;
        // дублировать её в поле с чужим смыслом нельзя — модель читает
        // `return_type` как объявленный тип возвращаемого значения.
        return_type: None,
        docstring,
        body,
        is_async: false, // BSL не имеет async
        node_hash,
        override_type,
        override_target,
    });

    // Рекурсивно обходим тело для извлечения вызовов (method_call в любых
    // выражениях: присваивания, условия, аргументы, операторы-вызовы).
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "parameters"
            | "annotation"
            | "EXPORT_KEYWORD"
            | "procedure_definition"
            | "function_definition" => {
                // Не рекурсируем в служебные узлы и вложенные определения
            }
            _ => {
                visit_body_for_calls(child, ctx, Some(&func_name), 1);
            }
        }
    }
}

/// Рекурсивный обход тела процедуры/функции для извлечения вызовов.
/// Ловит КАЖДЫЙ узел `method_call` — это реальный вызов `Имя(args)`; встречается
/// и как оператор-вызов, и внутри любых выражений (присваивания, условия,
/// аргументы). Имя вызываемой процедуры/функции — первый `identifier` внутри
/// method_call; квалификатор (`Модуль`) определяется в record_method_call через
/// родителя (`call_expression`/`access`) → `Модуль.Функция`.
fn visit_body_for_calls(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    current_func: Option<&str>,
    depth: usize,
) {
    if depth > MAX_VISIT_DEPTH {
        return;
    }
    if node.kind() == "method_call" {
        record_method_call(node, ctx, current_func);
    }
    // Всегда обходим детей: вложенные вызовы в аргументах (Ф(Г(х))) и любые
    // подвыражения тела.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_body_for_calls(child, ctx, current_func, depth + 1);
    }
}

/// Записать ребро вызова из узла `method_call`. callee — имя метода (первый
/// `identifier` внутри method_call), СКЛЕЕННОЕ с квалификатором-receiver, если
/// вызов идёт через точку (`Модуль.Метод` → `Модуль.Метод`). Квалификатор
/// вызова `ОбщийМодуль.Метод(1)` в грамматике tree-sitter-bsl:
///   call_expression
///     access( identifier «ОбщийМодуль» )
///     "."
///     method_call( identifier «Метод», arguments )
/// receiver — ПЕРВЫЙ именованный ребёнок родителя (узел access), когда
/// родитель — call_expression/access И у него больше одного именованного
/// ребёнка. Голый вызов `ГолыйВызов(2)` — method_call, чей родитель НЕ
/// call_expression/access, либо родитель access с единственным именованным
/// ребёнком (голова цепочки `Ф().Метод()`). caller — имя процедуры-контейнера.
fn record_method_call(node: tree_sitter::Node, ctx: &mut VisitContext, current_func: Option<&str>) {
    if let Some(callee) = callee_of(node, ctx.source) {
        ctx.calls.push(ParsedCall {
            caller: current_func.unwrap_or("<module>").to_string(),
            callee,
            line: node.start_position().row + 1,
        });
    }
}

/// Имя вызываемого из узла `method_call` — ровно то, что попадает в граф
/// вызовов: голое имя либо `Квалификатор.Метод`, если вызов идёт через цепочку
/// имён. Вынесено отдельно, потому что тем же правилом пользуется структурный
/// разбор (`parse_structure`): имена в контексте вызовов обязаны совпадать с
/// именами в графе, иначе контекст не найдётся.
fn callee_of(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let ident = find_child_by_kind(node, "identifier")?;
    let method = node_text(ident, source).to_string();
    if method.is_empty() {
        return None;
    }

    let callee = match node.parent() {
        Some(parent)
            if matches!(parent.kind(), "call_expression" | "access")
                && parent.named_child_count() > 1 =>
        {
            let receiver = parent.named_child(0).expect("named_child_count() > 1");
            let q = node_text(receiver, source);
            // Квалификатор — только цепочка имён (`ОбщегоНазначения`,
            // `Справочники.Номенклатура`). Получатель-выражение
            // (`Ф(1).Метод()`, `Массив[0].Метод()`) в имя не идёт: там
            // оказывался кусок исходника со скобками и переносами строк,
            // который поиск по графу не находил никогда.
            if is_plain_qualifier(q) {
                format!("{q}.{method}")
            } else {
                method
            }
        }
        _ => method,
    };
    Some(callee)
}

/// Обработать var_definition — объявление переменной(ых) модуля.
/// `Перем А, Б;` даёт ОДИН var_definition с ДВУМЯ дочерними `variable_spec`
/// (по одному на имя) — в отличие от старой грамматики, где на одно
/// объявление приходилась одна переменная. Экспорт — общий для всего
/// объявления (поле `export` у var_definition).
fn visit_module_var(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;

    let value = if node.child_by_field_name("export").is_some() {
        Some("Экспорт".to_string())
    } else {
        None
    };

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "variable_spec" {
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = node_text(name_node, source).to_string();
                if !name.is_empty() {
                    let line = child.start_position().row + 1;
                    ctx.variables.push(ParsedVariable {
                        name,
                        value: value.clone(),
                        line,
                    });
                }
            }
        }
    }
}

/// Главная функция парсинга BSL-файла.
///
/// Обе защиты (отсев двоичного ввода и дедлайн разбора) живут в `parser::types`
/// и общие для всех языков. Здесь проверка двоичности повторяется намеренно:
/// `parse` могут звать напрямую, минуя `parse_guarded`.
fn parse_bsl(source: &str) -> Result<ParseResult> {
    // Защита 1: двоичный .bsl (EDT выгружает защищённые модули поставщика как
    // `ObjectModule.bsl` с двоичным образом 1С) — не отдаём в tree-sitter,
    // иначе он деградирует на бесструктурном вводе и вешает индексацию.
    if looks_binary(source) {
        return Ok(empty_parse_result(source));
    }

    let mut ts_parser = tree_sitter::Parser::new();
    ts_parser
        .set_language(&tree_sitter_bsl::LANGUAGE.into())
        .map_err(|e| anyhow!("Ошибка установки языка tree-sitter-bsl: {}", e))?;

    // Защита 2: дедлайн на парсинг — страховка от любой будущей патологии.
    // При превышении parse() возвращает None; трактуем как пустой результат,
    // чтобы один файл не ронял ошибкой и не вешал всю индексацию.
    #[allow(deprecated)]
    ts_parser.set_timeout_micros(PARSE_TIMEOUT_MS * 1000);

    // Нормализация обходит 6 дефектов грамматики tree-sitter-bsl 0.1.7 (буква ё,
    // тернарный оператор `? (`, `ВызватьИсключение;` без аргумента вне первого
    // оператора Исключения, неразрывный пробел, `# Если`, отрицательное значение
    // параметра по умолчанию). Замены побайтовые с сохранением длины, поэтому
    // смещения узлов совпадают с оригиналом — тексты узлов ниже читаем из
    // ОРИГИНАЛЬНОГО source, а не из нормализованной копии. Без этой нормализации
    // имена рвутся (например, «СчётаУчёта» → «Сч») и часть объявлений теряется.
    let for_parser = bsl_parse::normalize_for_parser(source);

    let tree = match ts_parser.parse(for_parser.as_ref(), None) {
        Some(t) => t,
        None => return Ok(empty_parse_result(source)),
    };

    let root = tree.root_node();
    let source_bytes = source.as_bytes();

    // Количество строк
    let lines_total = source.lines().count();

    // Обход AST
    let mut ctx = VisitContext::new(source_bytes);
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        visit_node(child, &mut ctx, None, 0);
    }

    Ok(ParseResult {
        functions: ctx.functions,
        classes: Vec::new(), // BSL не имеет классов
        imports: Vec::new(), // BSL не имеет импортов
        calls: ctx.calls,
        variables: ctx.variables,
        lines_total,
    })
}

/// Вид структурного блока модуля.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BslBlockKind {
    /// Цикл: `Для … По … Цикл`, `Для Каждого … Из … Цикл`, `Пока … Цикл`.
    Loop,
    /// `Попытка … Исключение … КонецПопытки` целиком, вместе с обработчиком.
    Try,
    /// `Если … Тогда … КонецЕсли` вместе со всеми `ИначеЕсли` и `Иначе`.
    If,
}

/// Блок кода со своими границами и глубиной вложенности.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BslBlock {
    pub kind: BslBlockKind,
    /// Строки считаются от 1, границы включительные с обеих сторон.
    pub line_start: usize,
    pub line_end: usize,
    /// 0 — тело процедуры или модуль верхнего уровня.
    pub depth: u8,
}

/// Вызов вместе с контекстом: где именно он стоит в коде.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BslCallSite {
    pub caller: String,
    pub callee: String,
    pub line: usize,
    pub in_loop: bool,
    pub in_exception: bool,
    /// Глубина вложенности блоков на строке вызова.
    pub depth: u8,
}

/// Структурный разбор модуля: блоки и вызовы с контекстом.
#[derive(Debug, Clone, Default)]
pub struct BslStructure {
    pub blocks: Vec<BslBlock>,
    pub calls: Vec<BslCallSite>,
}

impl BslStructure {
    /// Контекст строки: попадает ли она в цикл, в `Попытку` и как глубоко
    /// вложена. Берётся самый внутренний блок, содержащий строку.
    pub fn context_at(&self, line: usize) -> (bool, bool, u8) {
        let mut in_loop = false;
        let mut in_exception = false;
        let mut deepest: Option<u8> = None;
        for b in &self.blocks {
            if b.line_start <= line && line <= b.line_end {
                match b.kind {
                    BslBlockKind::Loop => in_loop = true,
                    BslBlockKind::Try => in_exception = true,
                    BslBlockKind::If => {}
                }
                deepest = Some(match deepest {
                    Some(d) => d.max(b.depth),
                    None => b.depth,
                });
            }
        }
        (in_loop, in_exception, deepest.map_or(0, |d| d + 1))
    }
}

/// Разобрать структуру модуля BSL: блоки и контекст вызовов.
///
/// Отдельный публичный вход для структурных инструментов (`query_call_sites`,
/// `get_node_structure`, `get_line_context`): они разбирают модуль по
/// требованию, и им нужен и скелет блоков, и привязка вызовов к блокам. Разбор
/// здесь ровно один — второй проход по тому же дереву стоил бы столько же,
/// сколько первый, а модуль полигона разбирается за 7,4 мс.
///
/// Ничего не пишет и не кэширует: кэш — забота вызывающего.
pub fn parse_structure(source: &str) -> BslStructure {
    // Те же защиты, что и у индексации: двоичный модуль поставщика и дедлайн.
    if looks_binary(source) {
        return BslStructure::default();
    }
    let mut ts_parser = tree_sitter::Parser::new();
    if ts_parser
        .set_language(&tree_sitter_bsl::LANGUAGE.into())
        .is_err()
    {
        return BslStructure::default();
    }
    #[allow(deprecated)]
    ts_parser.set_timeout_micros(PARSE_TIMEOUT_MS * 1000);
    let for_parser = bsl_parse::normalize_for_parser(source);
    let Some(tree) = ts_parser.parse(for_parser.as_ref(), None) else {
        return BslStructure::default();
    };

    /// Кадр обхода: узел и накопленный контекст.
    struct Frame<'a> {
        node: tree_sitter::Node<'a>,
        depth: u8,
        proc: Option<String>,
        loops: u32,
        tries: u32,
    }

    let source_bytes = source.as_bytes();
    let mut out = BslStructure::default();
    let root = tree.root_node();

    let mut stack: Vec<Frame> = Vec::new();
    let mut cursor = root.walk();
    let top: Vec<Frame> = root
        .children(&mut cursor)
        .map(|node| Frame {
            node,
            depth: 0,
            proc: None,
            loops: 0,
            tries: 0,
        })
        .collect();
    stack.extend(top.into_iter().rev());

    while let Some(frame) = stack.pop() {
        let (mut depth, mut loops, mut tries) = (frame.depth, frame.loops, frame.tries);
        let mut proc = frame.proc.clone();
        let node = frame.node;
        let start = node.start_position().row + 1;
        let end = node.end_position().row + 1;

        match node.kind() {
            "procedure_definition" | "function_definition" => {
                proc = node
                    .child_by_field_name("name")
                    .map(|n| node_text(n, source_bytes).to_string());
            }
            "for_statement" | "for_each_statement" | "while_statement" => {
                out.blocks.push(BslBlock {
                    kind: BslBlockKind::Loop,
                    line_start: start,
                    line_end: end,
                    depth,
                });
                depth = depth.saturating_add(1);
                loops += 1;
            }
            "try_statement" => {
                out.blocks.push(BslBlock {
                    kind: BslBlockKind::Try,
                    line_start: start,
                    line_end: end,
                    depth,
                });
                depth = depth.saturating_add(1);
                tries += 1;
            }
            "if_statement" => {
                out.blocks.push(BslBlock {
                    kind: BslBlockKind::If,
                    line_start: start,
                    line_end: end,
                    depth,
                });
                depth = depth.saturating_add(1);
            }
            "method_call" => {
                if let Some(callee) = callee_of(node, source_bytes) {
                    out.calls.push(BslCallSite {
                        caller: proc.clone().unwrap_or_else(|| "<module>".to_string()),
                        callee,
                        line: start,
                        in_loop: loops > 0,
                        in_exception: tries > 0,
                        depth,
                    });
                }
            }
            _ => {}
        }

        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(Frame {
                node: child,
                depth,
                proc: proc.clone(),
                loops,
                tries,
            });
        }
    }

    out.blocks.sort_by_key(|b| (b.line_start, b.line_end));
    out.calls.sort_by_key(|c| c.line);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::LanguageParser;

    /// Структурный разбор: блоки, контекст вызовов и совпадение имён с графом.
    #[test]
    fn structure_reports_blocks_and_call_context() {
        let source = "Процедура Тест()\n    Подготовить();\n    Попытка\n        Для Каждого Строка Из Товары Цикл\n            Обработать(Строка);\n        КонецЦикла;\n    Исключение\n        ЗаписатьОшибку();\n    КонецПопытки;\n    ОбщегоНазначения.Сообщить(\"готово\");\nКонецПроцедуры";
        let s = parse_structure(source);

        let try_block = s
            .blocks
            .iter()
            .find(|b| b.kind == BslBlockKind::Try)
            .expect("Попытка должна быть блоком");
        assert_eq!(
            (try_block.line_start, try_block.line_end, try_block.depth),
            (3, 9, 0)
        );
        let loop_block = s
            .blocks
            .iter()
            .find(|b| b.kind == BslBlockKind::Loop)
            .expect("цикл должен быть блоком");
        assert_eq!(
            (loop_block.line_start, loop_block.line_end, loop_block.depth),
            (4, 6, 1)
        );

        let call = |name: &str| {
            s.calls
                .iter()
                .find(|c| c.callee == name)
                .unwrap_or_else(|| panic!("нет вызова {name}"))
        };

        let plain = call("Подготовить");
        assert_eq!(
            (plain.line, plain.in_loop, plain.in_exception, plain.depth),
            (2, false, false, 0)
        );

        let in_both = call("Обработать");
        assert_eq!(
            (
                in_both.line,
                in_both.in_loop,
                in_both.in_exception,
                in_both.depth
            ),
            (5, true, true, 2)
        );

        let in_catch = call("ЗаписатьОшибку");
        assert_eq!(
            (
                in_catch.line,
                in_catch.in_loop,
                in_catch.in_exception,
                in_catch.depth
            ),
            (8, false, true, 1)
        );

        // Квалифицированный вызов остаётся таким же, как в графе вызовов.
        let qualified = call("ОбщегоНазначения.Сообщить");
        assert_eq!(qualified.depth, 0);
        assert!(!qualified.in_loop && !qualified.in_exception);

        // Вызыватель — процедура-контейнер, у всех вызовов одна.
        assert!(s.calls.iter().all(|c| c.caller == "Тест"));
    }

    /// Контекст строки: самый внутренний блок и признаки цикла и `Попытки`.
    #[test]
    fn structure_context_at_line() {
        // `Попытка` без `Исключение` — невалидный BSL: грамматика такой блок не
        // собирает, и тест на нём проверял бы не то, что задумано.
        let source = "Процедура Тест()\n    Для Каждого Строка Из Товары Цикл\n        Попытка\n            Обработать(Строка);\n        Исключение\n            ЗаписатьОшибку();\n        КонецПопытки;\n    КонецЦикла;\nКонецПроцедуры";
        let s = parse_structure(source);
        assert_eq!(s.context_at(1), (false, false, 0));
        assert_eq!(s.context_at(2), (true, false, 1));
        assert_eq!(s.context_at(4), (true, true, 2));
        assert_eq!(s.context_at(9), (false, false, 0));
    }

    /// Двоичный модуль поставщика не разбирается — как и при индексации.
    #[test]
    fn structure_skips_binary_module() {
        let s = parse_structure("\u{0}\u{1}\u{2}бинарный");
        assert!(s.blocks.is_empty());
        assert!(s.calls.is_empty());
    }

    #[test]
    fn test_parse_bsl_procedure() {
        let parser = BslParser::new();
        let source = "Процедура ПриОткрытии()\n    Сообщить(\"Привет\");\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        assert_eq!(result.functions[0].name, "ПриОткрытии");
    }

    #[test]
    fn test_parse_bsl_function() {
        let parser = BslParser::new();
        let source = "Функция ПолучитьДанные(Параметр1, Параметр2) Экспорт\n    Возврат Параметр1 + Параметр2;\nКонецФункции";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        assert_eq!(result.functions[0].name, "ПолучитьДанные");
    }

    #[test]
    fn test_parse_bsl_with_directive() {
        let parser = BslParser::new();
        let source = "&НаСервере\nПроцедура ОбработатьНаСервере()\n    // код\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        assert_eq!(result.functions[0].name, "ОбработатьНаСервере");
    }

    #[test]
    fn test_parse_bsl_calls() {
        let parser = BslParser::new();
        let source = "Процедура Тест()\n    Сообщить(\"Привет\");\n    Рез = ОбщегоНазначения.ЗначениеРеквизита(Объект);\n    ОбщийМодуль.МетодМодуля();\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        let names: Vec<&str> = result.calls.iter().map(|c| c.callee.as_str()).collect();
        // Неквалифицированный вызов-оператор — остаётся голым именем (точки нет).
        assert!(
            names.contains(&"Сообщить"),
            "Сообщить не найден: {:?}",
            names
        );
        // Главный кейс: вызов ФУНКЦИИ в ВЫРАЖЕНИИ через общий модуль —
        // квалификатор приклеен: `ОбщегоНазначения.ЗначениеРеквизита`.
        assert!(
            names.contains(&"ОбщегоНазначения.ЗначениеРеквизита"),
            "склеенный вызов функции в выражении не найден: {:?}",
            names
        );
        // Квалифицированный вызов-процедура (call_statement) — тоже склеен.
        assert!(
            names.contains(&"ОбщийМодуль.МетодМодуля"),
            "склеенный МетодМодуля не найден: {:?}",
            names
        );
        // Голый метод без модуля не должен появляться отдельно для
        // квалифицированных вызовов (квалификатор приклеен).
        assert!(
            !names.contains(&"ЗначениеРеквизита") && !names.contains(&"МетодМодуля"),
            "квалифицированный вызов попал в callee голым: {:?}",
            names
        );
        // Имена модулей-приёмников не должны попадать в callee как отдельные «вызовы».
        assert!(
            !names.contains(&"ОбщегоНазначения") && !names.contains(&"ОбщийМодуль"),
            "имя модуля ошибочно записано как отдельный вызов: {:?}",
            names
        );
    }

    #[test]
    fn test_parse_bsl_calls_qualified_and_bare() {
        // Простейший прямой случай из плана миграции: один квалифицированный
        // и один голый вызов, оба операторы верхнего уровня тела процедуры.
        let parser = BslParser::new();
        let source =
            "Процедура Тест()\n    ОбщийМодуль.Метод(1);\n    ГолыйВызов(2);\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        let names: Vec<&str> = result.calls.iter().map(|c| c.callee.as_str()).collect();
        assert!(names.contains(&"ОбщийМодуль.Метод"), "callee: {:?}", names);
        assert!(names.contains(&"ГолыйВызов"), "callee: {:?}", names);
    }

    /// Квалификатор приклеивается только когда получатель — имя. Получатель-
    /// выражение (`Ф(1).Метод()`) прежде давал callee `Ф(1).Метод` — кусок
    /// исходника, который поиск по графу не находил.
    #[test]
    fn test_parse_bsl_calls_receiver_expression_not_qualified() {
        let parser = BslParser::new();
        let source = "Процедура Тест()\n    Ф(1).Метод();\n    Массив[0].Другой();\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        let names: Vec<&str> = result.calls.iter().map(|c| c.callee.as_str()).collect();

        assert!(names.contains(&"Метод"), "callee: {names:?}");
        assert!(names.contains(&"Другой"), "callee: {names:?}");
        assert!(
            !names.iter().any(|n| n.contains('(') || n.contains('[')),
            "в callee попал текст выражения: {names:?}"
        );
    }

    #[test]
    fn test_parse_bsl_module_vars() {
        let parser = BslParser::new();
        let source = "Перем МояПеременная Экспорт;\nПерем ВтораяПеременная;\n\nПроцедура Тест()\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert!(result.variables.len() >= 2);
    }

    #[test]
    fn test_parse_bsl_multiple_vars_single_declaration() {
        // `Перем А, Б;` — одно объявление, но ДВЕ переменные (variable_spec
        // каждая). В старой грамматике на объявление приходилась одна переменная.
        let parser = BslParser::new();
        let source = "Перем А, Б;";
        let result = parser.parse(source, "test.bsl").unwrap();
        let names: Vec<&str> = result.variables.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["А", "Б"], "переменные: {:?}", names);
    }

    #[test]
    fn test_parse_bsl_var_export_value() {
        let parser = BslParser::new();
        let source = "Перем Г Экспорт;";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.variables.len(), 1);
        assert_eq!(result.variables[0].value.as_deref(), Some("Экспорт"));
    }

    #[test]
    fn test_parse_bsl_english_keywords() {
        let parser = BslParser::new();
        let source = "Procedure OnOpen() Export\n    Message(\"Hello\");\nEndProcedure";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        assert_eq!(result.functions[0].name, "OnOpen");
    }

    #[test]
    fn test_parse_bsl_export_marker() {
        let parser = BslParser::new();
        // Функция с Экспорт — в args должен быть суффикс "Экспорт"
        let source = "Функция ПолучитьДанные(Пар1) Экспорт\n    Возврат Пар1;\nКонецФункции";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        let f = &result.functions[0];
        assert!(f.args.as_deref().unwrap_or("").contains("Экспорт"));
    }

    #[test]
    fn test_parse_bsl_directive_in_docstring() {
        let parser = BslParser::new();
        let source = "&НаКлиенте\nПроцедура НаКлиенте()\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        let f = &result.functions[0];
        // Директива компиляции хранится в docstring…
        assert!(
            f.docstring.as_deref().unwrap_or("").contains("&НаКлиенте"),
            "директива не попала в docstring: {:?}",
            f.docstring
        );
        // …и НЕ подменяет собой тип возвращаемого значения
        assert!(
            f.return_type.is_none(),
            "директива не должна лежать в return_type: {:?}",
            f.return_type
        );
    }

    #[test]
    fn test_parse_bsl_docstring_contains_type() {
        let parser = BslParser::new();
        let source = "Процедура МояПроц()\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        // docstring содержит "procedure"
        let doc = result.functions[0].docstring.as_deref().unwrap_or("");
        assert!(doc.contains("procedure"));
    }

    #[test]
    fn test_parse_bsl_override_before() {
        let parser = BslParser::new();
        let source = "&Перед(\"ОригинальнаяПроцедура\")\nПроцедура Ext_ОригинальнаяПроцедура()\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        let f = &result.functions[0];
        assert_eq!(f.override_type.as_deref(), Some("Перед"));
        assert_eq!(f.override_target.as_deref(), Some("ОригинальнаяПроцедура"));
        // Директива тоже должна быть извлечена — в docstring
        assert!(
            f.docstring.as_deref().unwrap_or("").contains("&Перед"),
            "директива не попала в docstring: {:?}",
            f.docstring
        );
    }

    #[test]
    fn test_parse_bsl_override_instead() {
        let parser = BslParser::new();
        let source = "&Вместо(\"ПолучитьДанные\")\nФункция Ext_ПолучитьДанные()\n    Возврат 42;\nКонецФункции";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        let f = &result.functions[0];
        assert_eq!(f.override_type.as_deref(), Some("Вместо"));
        assert_eq!(f.override_target.as_deref(), Some("ПолучитьДанные"));
    }

    #[test]
    fn test_parse_bsl_override_after() {
        let parser = BslParser::new();
        let source = "&После(\"ПриЗаписи\")\nПроцедура Ext_ПриЗаписи()\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        let f = &result.functions[0];
        assert_eq!(f.override_type.as_deref(), Some("После"));
        assert_eq!(f.override_target.as_deref(), Some("ПриЗаписи"));
    }

    #[test]
    fn test_parse_bsl_no_override_for_regular_directive() {
        let parser = BslParser::new();
        let source = "&НаСервере\nПроцедура Тест()\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        let f = &result.functions[0];
        assert!(f.override_type.is_none());
        assert!(f.override_target.is_none());
        assert!(
            f.docstring.as_deref().unwrap_or("").contains("&НаСервере"),
            "директива не попала в docstring: {:?}",
            f.docstring
        );
    }

    #[test]
    fn test_parse_bsl_normalizes_yo_letter() {
        // Буква «ё» не входит в диапазон идентификатора грамматики tree-sitter-bsl
        // (`[\wа-я_]`) — без нормализации имя рвётся на «Сч». Проверяем, что
        // normalize_for_parser в parse_bsl восстанавливает полное имя.
        let parser = BslParser::new();
        let source = "Процедура СчётаУчёта()\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(
            result.functions.len(),
            1,
            "функция не найдена: {:?}",
            result.functions
        );
        assert_eq!(result.functions[0].name, "СчётаУчёта");
    }

    #[test]
    fn test_binary_source_yields_empty_not_hang() {
        // .bsl с двоичным содержимым (EDT-защищённый модуль поставщика) не должен
        // парситься tree-sitter'ом: возвращаем пустой результат, без зависания.
        let parser = BslParser::new();
        let binary = "\u{0}\u{2}binary\u{0}image content";
        let result = parser.parse(binary, "ObjectModule.bsl").unwrap();
        assert_eq!(result.functions.len(), 0);
    }
}

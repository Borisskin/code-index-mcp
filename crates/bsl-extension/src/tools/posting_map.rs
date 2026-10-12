// MCP-tool `posting_map` — карта механизма проведения документа 1С.
//
// Отвечает на вопрос «как формируются движения документа и где именно в коде это
// происходит», склеивая четыре слоя индекса:
//
//   1. `declared` — регистры декларативного состава движений
//      (`data_links.link_kind = 'recorder'`, документ → регистр);
//   2. `path`     — файлы и процедуры, достижимые из обработчика проведения по
//      графу вызовов (`proc_call_graph`) вглубь на `depth`;
//   3. `writes`   — ФАКТИЧЕСКИЕ записи движений: вызовы
//      `Движения.<Регистр>.Добавить` / `.Записать` (действие по конкретному
//      регистру, `level = "register"`), а также `Движения.Записать` (запись всего
//      набора) и `Движения.Добавить` (добавление набора в коллекцию по имени —
//      у обоих `level = "collection"`), атрибутированные процедуре по диапазону
//      строк `functions.line_start..line_end`;
//   4. `mentions` — где объявленные регистры упоминаются в коде
//      (`metadata_code_usages`), ограниченно достижимыми файлами. Это упоминание,
//      а не запись: в модуле менеджера так выглядит и чтение остатков.
//
// Зачем `writes` отдельным слоем. По замеру на живой конфигурации 72 % записей
// движений живут в общих модулях подсистем учёта (803 вызова `Движения.*` против
// 184 в модулях объектов и 131 в модулях менеджера), а у отдельных документов
// (ввод начальных остатков, корректировки регистров) движения формирует именно
// модуль менеджера. Слой `mentions` модуль менеджера исключает по умолчанию —
// как источник запросов остатков, — и без `writes` эти записи терялись молча.
// Теперь фильтр по типу модуля действует только на упоминания, а записи видны
// везде: они точны независимо от того, в каком модуле лежат.
//
// Чего инструмент не делает: в слое `writes` чтение от записи отличается
// (`.Найти`, `.Количество`, `.ДополнительныеСвойства` отсеиваются), в слое
// `mentions` разделения нет — там любое обращение к регистру. Динамика (имя
// регистра в строке, `Выполнить`) не восстанавливается.
//
// Краевые случаи (все встречены на реальных данных):
//   * у документа может не быть `ОбработкаПроведения` в модулях объекта и
//     менеджера — тогда `entry_point` пуст, `heuristic = true`, ограничение по
//     графу снимается (берутся все файлы, кроме форм и переопределяемых
//     модулей), а слой `writes` продолжает работать: записи видны и без графа;
//   * регистр может не встречаться в коде вообще (движения формирует
//     универсальный механизм по имени из метаданных) — такие регистры попадают
//     в `registers_without_code`, и это содержательный ответ, а не ошибка.
//
// Защита контекста: `declared` ≤ `CAP_DECLARED`, `path` ≤ `CAP_PATH` файлов и
// процедур, `writes` ≤ `CAP_WRITES` строк, `mentions` ≤ `CAP_MENTIONS` строк.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;

use code_index_core::extension::{IndexTool, ToolContext};
use rusqlite::{params, params_from_iter, OptionalExtension};
use serde_json::{json, Value};

/// Потолок регистров в блоке `declared`.
const CAP_DECLARED: i64 = 500;
/// Потолок файлов и процедур в блоке `path`.
const CAP_PATH: usize = 200;
/// Потолок строк в блоке `mentions` по умолчанию.
const CAP_MENTIONS: i64 = 200;
/// Потолок строк в блоке `writes`.
const CAP_WRITES: usize = 200;
/// Потолок строк вызовов, вычитываемых из `calls` одним запросом (страховка от
/// репозитория, где `Движения.*` зовут десятки тысяч раз; на живой конфигурации
/// таких вызовов около тысячи).
const CAP_CALL_ROWS: i64 = 20_000;
/// Глубина обхода графа вызовов по умолчанию и её максимум.
const DEFAULT_DEPTH: i64 = 3;
const MAX_DEPTH: i64 = 6;
/// Имя обработчика проведения, который пробуем как точку входа первым.
const POSTING_HANDLER: &str = "ОбработкаПроведения";
/// Члены вызова, которыми движение записывается в набор (в нижнем регистре:
/// сравнение регистронезависимое — в BSL регистр не различается).
const WRITE_MEMBERS: [&str; 2] = ["добавить", "записать"];

pub struct PostingMapTool;

/// Строка фактической записи движения. `register = None` — действие по всей
/// коллекции движений: `Движения.Записать` (запись всего набора) или
/// `Движения.Добавить` (добавление набора в коллекцию по имени регистра — имя
/// лежит в аргументе вызова и статически недоступно).
struct WriteRow {
    register: Option<String>,
    member: String,
    procedure: String,
    file: String,
    line: i64,
    hits: i64,
}

/// Строка упоминания регистра в коде: регистр → процедура → файл.
struct MentionRow {
    register: String,
    procedure: String,
    file: String,
    hits: i64,
    first_line: i64,
}

/// Результат слоя фактических записей.
struct Writes {
    rows: Vec<WriteRow>,
    truncated: bool,
}

/// Результат слоя упоминаний.
struct Mentions {
    rows: Vec<MentionRow>,
    truncated: bool,
}

/// Область поиска фактических записей.
enum WriteScope<'a> {
    /// Обход графа нашёл точку входа: смотрим только достижимые файлы.
    Files(&'a HashSet<String>),
    /// Точки входа нет: все файлы, кроме форм и переопределяемых модулей.
    AllExceptForms,
}

impl IndexTool for PostingMapTool {
    fn name(&self) -> &str {
        "posting_map"
    }

    fn description(&self) -> &str {
        "Карта механизма проведения документа 1С: что документ пишет по метаданным \
         (recorder-рёбра), какой код достижим от обработчика проведения по графу \
         вызовов, где в этом коде движения ФАКТИЧЕСКИ записываются и где регистры \
         лишь упоминаются. Слой 'writes' — вызовы 'Движения.<Регистр>.Добавить' / \
         '.Записать' (level=register), 'Движения.Записать' (запись всего набора, \
         whole_set=true) и 'Движения.Добавить' (level=collection) с именем \
         процедуры, файла и строки; слой 'mentions' — обращения к регистрам из metadata_code_usages, \
         ограниченные достижимыми файлами (модуль менеджера по умолчанию исключён: \
         там запросы остатков). Слои не смешиваются: упоминание — не запись. Если \
         обработчика проведения в модулях объекта и менеджера нет, возвращает \
         heuristic=true и строит карту без ограничения по графу (записи видны и без \
         него). Регистры без обращений в коде попадают в registers_without_code: \
         движения формирует универсальный механизм по имени из метаданных. \
         For BSL/1C repositories only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Алиас репозитория" },
                "object": {
                    "type": "string",
                    "description": "Документ, например 'Document.РеализацияТоваровУслуг'"
                },
                "depth": {
                    "type": "integer",
                    "description": "Глубина обхода графа вызовов от обработчика проведения, 1..6 (по умолчанию 3)"
                },
                "include_manager_module": {
                    "type": "boolean",
                    "description": "Включать модуль менеджера в слой УПОМИНАНИЙ (по умолчанию false: он даёт запросы остатков). На слой writes не влияет — фактические записи в модуле менеджера видны всегда"
                },
                "limit": {
                    "type": "integer",
                    "description": "Потолок строк mentions (по умолчанию 200, максимум 200)"
                }
            },
            "required": ["repo", "object"]
        })
    }

    fn applicable_languages(&self) -> Option<&'static [&'static str]> {
        Some(&["bsl"])
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Value> + Send + 'a>> {
        Box::pin(async move {
            let object = match crate::tools::object_value(&args) {
                Some(s) => crate::code_usages::normalize_object_ref(s).into_owned(),
                None => {
                    return crate::tools::wrap_error(json!({
                        "error": "missing required parameter 'object' (string) — ожидается документ, например 'Document.РеализацияТоваровУслуг'"
                    }));
                }
            };

            let depth = args
                .get("depth")
                .and_then(Value::as_i64)
                .unwrap_or(DEFAULT_DEPTH)
                .clamp(1, MAX_DEPTH);
            let include_manager_module = args
                .get("include_manager_module")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let limit = args
                .get("limit")
                .and_then(Value::as_i64)
                .filter(|n| *n > 0)
                .unwrap_or(CAP_MENTIONS)
                .min(CAP_MENTIONS);

            let storage = match ctx.storage.get().await {
                Ok(s) => s,
                Err(e) => {
                    return crate::tools::wrap_error(json!({
                        "error": format!("storage pool: {}", e)
                    }));
                }
            };
            let conn = storage.conn();

            // Имя приводим к записи из конфигурации: иначе иной регистр кириллицы
            // молча даёт пустые блоки («движений нет»).
            let object = crate::tools::canonical_object_name(conn, &object);

            let (declared, declared_truncated) = match declared_registers(conn, &object) {
                Ok(v) => v,
                Err(e) => {
                    return crate::tools::wrap_error(json!({
                        "error": format!("database error (declared): {}", e)
                    }))
                }
            };

            let mut warnings: Vec<String> = Vec::new();
            if declared.is_empty() {
                warnings.push(format!(
                    "у объекта '{}' нет декларативных движений (recorder-рёбер): либо это не документ, либо состав движений не заполнен в метаданных",
                    object
                ));
            }

            let entry = match resolve_entry_point(conn, &object, &mut warnings) {
                Ok(v) => v,
                Err(e) => {
                    return crate::tools::wrap_error(json!({
                        "error": format!("database error (entry point): {}", e)
                    }))
                }
            };

            let (reach_files, reach_procs) = match &entry {
                Some(key) => match reachable(conn, key, depth) {
                    Ok(v) => v,
                    Err(e) => {
                        return crate::tools::wrap_error(json!({
                            "error": format!("database error (call graph): {}", e)
                        }))
                    }
                },
                None => (Vec::new(), Vec::new()),
            };

            let heuristic = entry.is_none();
            if heuristic {
                warnings.push(format!(
                    "обработчик проведения ('{}') не найден ни в модуле объекта, ни в модуле менеджера: код проведения лежит в общем модуле, в модуле расширения или отсутствует в выгрузке; карта построена без ограничения по графу вызовов (исключены формы и переопределяемые модули), а слой записей показывает все записи по объявленным регистрам",
                    POSTING_HANDLER
                ));
            }

            let reach_set: HashSet<String> = reach_files.iter().cloned().collect();
            let scope = if heuristic {
                WriteScope::AllExceptForms
            } else {
                WriteScope::Files(&reach_set)
            };

            let writes = match collect_writes(conn, &declared, scope, CAP_WRITES) {
                Ok(v) => v,
                Err(e) => {
                    return crate::tools::wrap_error(json!({
                        "error": format!("database error (writes): {}", e)
                    }))
                }
            };

            let mentions = match build_mentions(
                conn,
                &object,
                &reach_files,
                include_manager_module,
                heuristic,
                limit,
            ) {
                Ok(v) => v,
                Err(e) => {
                    return crate::tools::wrap_error(json!({
                        "error": format!("database error (mentions): {}", e)
                    }))
                }
            };

            // Регистры без обращений в коде — содержательный ответ: движения
            // формирует универсальный механизм по имени регистра из метаданных.
            let untouched = registers_without_code(&declared, &writes.rows, &mentions.rows);

            // Счётчики считаем ДО перемещения векторов в json! — модель цитирует
            // готовое число, а не пересчитывает массив.
            let declared_count = declared.len();
            let files_count = reach_files.len();
            let procs_count = reach_procs.len();
            let files_truncated = files_count > CAP_PATH;
            let procs_truncated = procs_count > CAP_PATH;
            let writes_count = writes.rows.len();
            let whole_set_count = writes
                .rows
                .iter()
                .filter(|r| r.register.is_none() && r.member == "Записать")
                .count();
            let mentions_count = mentions.rows.len();

            let writes_json: Vec<Value> = writes
                .rows
                .into_iter()
                .map(|r| {
                    // Уровень действия: по конкретному регистру (`Движения.Рег.Добавить`)
                    // или по всей коллекции (`Движения.Записать` — запись всего набора,
                    // `Движения.Добавить` — добавление набора в коллекцию по имени).
                    let level = if r.register.is_none() {
                        "collection"
                    } else {
                        "register"
                    };
                    let whole_set = r.register.is_none() && r.member == "Записать";
                    json!({
                        "register": r.register,
                        "level": level,
                        "whole_set": whole_set,
                        "member": r.member,
                        "procedure": r.procedure,
                        "file": r.file,
                        "line": r.line,
                        "hits": r.hits,
                    })
                })
                .collect();
            let mentions_json: Vec<Value> = mentions
                .rows
                .into_iter()
                .map(|r| {
                    json!({
                        "register": r.register,
                        "procedure": r.procedure,
                        "file": r.file,
                        "hits": r.hits,
                        "first_line": r.first_line,
                    })
                })
                .collect();

            crate::tools::wrap_with_meta(
                "posting_map",
                json!({
                    "object": object,
                    "depth": depth,
                    "scope": if heuristic { "heuristic" } else { "call_graph" },
                    "entry_point": entry,
                    "heuristic": heuristic,
                    "declared": {
                        "registers": declared,
                        "count": declared_count,
                        "truncated": declared_truncated,
                    },
                    "path": {
                        "files": reach_files.iter().take(CAP_PATH).collect::<Vec<_>>(),
                        "files_count": files_count,
                        "files_truncated": files_truncated,
                        "procedures": reach_procs.iter().take(CAP_PATH).collect::<Vec<_>>(),
                        "procedures_count": procs_count,
                        "procedures_truncated": procs_truncated,
                    },
                    "writes": {
                        "rows": writes_json,
                        "count": writes_count,
                        "whole_set_count": whole_set_count,
                        "truncated": writes.truncated,
                    },
                    "mentions": {
                        "rows": mentions_json,
                        "count": mentions_count,
                        "truncated": mentions.truncated,
                    },
                    "registers_without_code": untouched,
                    "warnings": warnings,
                }),
                Vec::new(),
            )
        })
    }
}

/// Регистры декларативного состава движений документа (recorder-рёбра).
/// Возвращает (имена регистров, truncated).
fn declared_registers(
    conn: &rusqlite::Connection,
    object: &str,
) -> rusqlite::Result<(Vec<String>, bool)> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT to_object FROM data_links \
         WHERE repo = 'default' AND link_kind = 'recorder' AND from_object = ?1 \
         ORDER BY to_object LIMIT ?2",
    )?;
    // CAP+1 — чтобы отличить «ровно CAP» от «есть ещё».
    let rows = stmt.query_map(params![object, CAP_DECLARED + 1], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    let truncated = out.len() as i64 > CAP_DECLARED;
    if truncated {
        out.truncate(CAP_DECLARED as usize);
    }
    Ok((out, truncated))
}

/// Модули объекта из `metadata_modules`: (module_type, code_path). Модуль объекта
/// идёт раньше модуля менеджера, основной конфигурации — раньше модулей расширений.
fn object_modules(
    conn: &rusqlite::Connection,
    object: &str,
) -> rusqlite::Result<Vec<(String, String)>> {
    let (meta_type, name) = match object.split_once('.') {
        Some(v) => v,
        None => return Ok(Vec::new()),
    };
    let folder = match crate::tools::meta_type_to_folder(meta_type) {
        Some(f) => f,
        None => return Ok(Vec::new()),
    };
    // object_name в metadata_modules — «папка во множественном числе» + имя.
    let owner = format!("{}.{}", folder, name);
    let mut stmt = conn.prepare(
        "SELECT module_type, code_path FROM metadata_modules \
         WHERE repo = 'default' AND object_name = ?1 AND code_path IS NOT NULL \
           AND module_type IN ('ObjectModule', 'ManagerModule') \
         ORDER BY CASE WHEN extension_name IS NULL OR extension_name = '' THEN 0 ELSE 1 END, \
                  CASE module_type WHEN 'ObjectModule' THEN 0 ELSE 1 END, code_path",
    )?;
    let rows = stmt.query_map(params![owner], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Точка входа обхода — `ОбработкаПроведения` в модуле объекта или менеджера.
/// Возвращает ключ процедуры `<путь>::<Имя>`.
///
/// «Похожую по имени» процедуру точкой входа не подставляем: на регламентной
/// операции такая подстановка дала карту из двух файлов и нуля записей при
/// 97 объявленных регистрах — ответ выглядел точным (`scope = call_graph`),
/// будучи неверным. Нет обработчика — значит эвристический режим, а похожая по
/// имени процедура остаётся подсказкой в `warnings`.
fn resolve_entry_point(
    conn: &rusqlite::Connection,
    object: &str,
    warnings: &mut Vec<String>,
) -> rusqlite::Result<Option<String>> {
    let modules = object_modules(conn, object)?;
    if modules.is_empty() {
        warnings.push(format!(
            "в metadata_modules нет модулей объекта '{}' — выгрузка неполная либо объект не проиндексирован",
            object
        ));
        return Ok(None);
    }
    for (_, path) in &modules {
        if let Some(name) = find_proc(conn, path, POSTING_HANDLER)? {
            return Ok(Some(format!("{}::{}", path, name)));
        }
    }
    for (module_type, path) in &modules {
        if let Some(name) = find_proc_like(conn, path)? {
            warnings.push(format!(
                "обработчик '{}' не найден; ближайшая по имени процедура — '{}' модуля {} ({})",
                POSTING_HANDLER, name, module_type, path
            ));
            break;
        }
    }
    Ok(None)
}

/// Первая процедура файла с точным именем. Порядок — по строке объявления.
fn find_proc(
    conn: &rusqlite::Connection,
    path: &str,
    name: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT f.name FROM functions f JOIN files fl ON fl.id = f.file_id \
         WHERE fl.path = ?1 AND f.name = ?2 ORDER BY f.line_start LIMIT 1",
        params![path, name],
        |r| r.get::<_, String>(0),
    )
    .optional()
}

/// Первая процедура файла, у которой «Проведен» в имени, — только для подсказки
/// в `warnings`, точкой входа она не становится.
fn find_proc_like(conn: &rusqlite::Connection, path: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT f.name FROM functions f JOIN files fl ON fl.id = f.file_id \
         WHERE fl.path = ?1 AND f.name LIKE '%Проведен%' ORDER BY f.line_start LIMIT 1",
        params![path],
        |r| r.get::<_, String>(0),
    )
    .optional()
}

/// Достижимые из точки входа файлы и имена процедур (обход в ширину средствами
/// recursive CTE). Возвращает (файлы, процедуры) — оба отсортированы и без дублей.
fn reachable(
    conn: &rusqlite::Connection,
    entry: &str,
    depth: i64,
) -> rusqlite::Result<(Vec<String>, Vec<String>)> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE chain(key, depth) AS ( \
             SELECT ?1, 0 \
             UNION \
             SELECT c.callee_proc_key, ch.depth + 1 \
               FROM proc_call_graph c JOIN chain ch ON c.caller_proc_key = ch.key \
              WHERE c.repo = 'default' AND c.callee_proc_key IS NOT NULL AND ch.depth < ?2 \
         ) \
         SELECT DISTINCT key FROM chain",
    )?;
    let rows = stmt.query_map(params![entry, depth], |r| r.get::<_, String>(0))?;
    let mut files: Vec<String> = Vec::new();
    let mut procs: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for row in rows {
        let key = row?;
        let (path, name) = split_proc_key(&key);
        if !path.is_empty() && seen.insert(path.clone()) {
            files.push(path);
        }
        if !name.is_empty() {
            procs.push(name);
        }
    }
    files.sort();
    procs.sort();
    procs.dedup();
    Ok((files, procs))
}

/// `<путь>::<Имя>` → (путь, имя). Ключ без разделителя остаётся путём.
fn split_proc_key(key: &str) -> (String, String) {
    match key.split_once("::") {
        Some((path, name)) => (path.to_string(), name.to_string()),
        None => (key.to_string(), String::new()),
    }
}

/// Разобрать имя вызова: `Движения.<Регистр>.<Член>` или `Движения.<Член>`.
/// Возвращает (имя регистра, член) только для записи движения — чтение и
/// настройка набора отсеиваются здесь, а не подстрочным фильтром.
fn parse_movement_call(callee: &str) -> Option<(Option<&str>, &str)> {
    let mut parts = callee.split('.');
    let head = parts.next()?;
    if head.to_lowercase() != "движения" {
        return None;
    }
    let second = parts.next()?;
    let third = parts.next();
    let member = third.unwrap_or(second);
    if !WRITE_MEMBERS.contains(&member.to_lowercase().as_str()) {
        return None;
    }
    // Глубже двух сегментов после `Движения` — это свойство набора
    // (`Движения.Хозрасчетный.ДополнительныеСвойства.Вставить`), не запись.
    if third.is_some() && parts.next().is_some() {
        return None;
    }
    Some(match third {
        Some(_) => (Some(second), member),
        None => (None, member),
    })
}

/// Фактические записи движений по объявленным регистрам документа.
///
/// Источник — таблица `calls` ядра: `Движения.<Регистр>.Добавить` хранится там
/// именем вызова целиком, поэтому имя регистра не нужно угадывать. Совпадение с
/// объявленным составом регистронезависимое (BSL), сравнение делается в Rust —
/// SQLite `lower()` кириллицу не сворачивает.
///
/// Вызовы по всей коллекции (`Движения.Записать` — запись набора, `Движения.Добавить`
/// — добавление набора по имени) к регистру не привязаны: они остаются в ответе
/// только из файлов, где уже найдены записи по регистрам этого документа, — иначе
/// в ответ попадали бы чужие документы.
fn collect_writes(
    conn: &rusqlite::Connection,
    declared: &[String],
    scope: WriteScope<'_>,
    limit: usize,
) -> rusqlite::Result<Writes> {
    let mut declared_by_name: HashMap<String, String> = HashMap::new();
    for full in declared {
        if let Some((_, name)) = full.split_once('.') {
            declared_by_name.insert(name.to_lowercase(), full.clone());
        }
    }

    let mut sql = String::from(
        "SELECT c.file_id, fl.path, c.callee, c.line, c.caller \
           FROM calls c JOIN files fl ON fl.id = c.file_id \
          WHERE c.callee LIKE 'Движения.%'",
    );
    let mut binds: Vec<rusqlite::types::Value> = Vec::new();
    match scope {
        WriteScope::Files(files) => {
            if files.is_empty() {
                return Ok(Writes {
                    rows: Vec::new(),
                    truncated: false,
                });
            }
            let placeholders = vec!["?"; files.len()].join(",");
            sql.push_str(&format!(" AND fl.path IN ({})", placeholders));
            let mut sorted: Vec<&String> = files.iter().collect();
            sorted.sort();
            for path in sorted {
                binds.push(rusqlite::types::Value::Text(path.clone()));
            }
        }
        WriteScope::AllExceptForms => {
            sql.push_str(
                " AND fl.path NOT LIKE '%/Forms/%' \
                  AND fl.path NOT LIKE '%/Form/%' \
                  AND fl.path NOT LIKE '%Переопределяемый%'",
            );
        }
    }
    sql.push_str(" LIMIT ?");
    binds.push(rusqlite::types::Value::Integer(CAP_CALL_ROWS));

    /// Одна найденная запись до агрегации.
    struct Pending {
        file_id: i64,
        path: String,
        register: Option<String>,
        member: String,
        line: i64,
        caller: String,
    }

    let mut pending: Vec<Pending> = Vec::new();
    {
        let mut stmt = conn.prepare(&sql)?;
        let raw = stmt.query_map(params_from_iter(binds.iter()), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        for row in raw {
            let (file_id, path, callee, line, caller) = row?;
            let Some((register, member)) = parse_movement_call(&callee) else {
                continue;
            };
            match register {
                Some(name) => match declared_by_name.get(&name.to_lowercase()) {
                    Some(full) => pending.push(Pending {
                        file_id,
                        path,
                        register: Some(full.clone()),
                        member: member.to_string(),
                        line,
                        caller,
                    }),
                    // Регистр, которого нет в объявленном составе этого документа.
                    None => continue,
                },
                None => pending.push(Pending {
                    file_id,
                    path,
                    register: None,
                    member: member.to_string(),
                    line,
                    caller,
                }),
            }
        }
    }

    // «Движения.Записать» оставляем только там, где пишутся регистры этого
    // документа: иначе в ответ попадут записи чужих документов.
    let files_with_registers: HashSet<String> = pending
        .iter()
        .filter(|p| p.register.is_some())
        .map(|p| p.path.clone())
        .collect();
    pending.retain(|p| p.register.is_some() || files_with_registers.contains(&p.path));

    // Процедура по диапазону строк: функции читаем один раз на файл, а не
    // запросом на каждую запись.
    let mut ids: Vec<i64> = pending.iter().map(|p| p.file_id).collect();
    ids.sort_unstable();
    ids.dedup();
    let mut by_file: HashMap<i64, Vec<(i64, i64, String)>> = HashMap::new();
    if !ids.is_empty() {
        let placeholders = vec!["?"; ids.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT file_id, name, line_start, line_end FROM functions \
             WHERE file_id IN ({})",
            placeholders
        ))?;
        let rows = stmt.query_map(params_from_iter(ids.iter()), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows {
            let (file_id, name, start, end) = row?;
            by_file.entry(file_id).or_default().push((start, end, name));
        }
    }

    // Агрегат: (регистр, член, процедура, файл) → (число вхождений, первая строка).
    let mut agg: BTreeMap<(String, String, String, String), (i64, i64)> = BTreeMap::new();
    for p in pending {
        let procedure = by_file
            .get(&p.file_id)
            .and_then(|fns| {
                fns.iter()
                    .filter(|(start, end, _)| *start <= p.line && p.line <= *end)
                    .max_by_key(|(start, _, _)| *start)
                    .map(|(_, _, name)| name.clone())
            })
            .unwrap_or_else(|| p.caller.clone());
        let key = (
            p.register.clone().unwrap_or_default(),
            p.member.clone(),
            procedure,
            p.path.clone(),
        );
        let entry = agg.entry(key).or_insert((0, p.line));
        entry.0 += 1;
        if p.line < entry.1 {
            entry.1 = p.line;
        }
    }

    let mut rows: Vec<WriteRow> = agg
        .into_iter()
        .map(
            |((register, member, procedure, file), (hits, line))| WriteRow {
                register: if register.is_empty() {
                    None
                } else {
                    Some(register)
                },
                member,
                procedure,
                file,
                line,
                hits,
            },
        )
        .collect();
    rows.sort_by(|a, b| {
        b.hits
            .cmp(&a.hits)
            .then_with(|| a.register.cmp(&b.register))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
    let truncated = rows.len() > limit;
    rows.truncate(limit);
    Ok(Writes { rows, truncated })
}

/// Карта «регистр → процедура → файл» по обращениям к регистрам в коде. Без
/// точки входа граф не ограничивает выборку: тогда (и только тогда) отсекаем
/// формы и переопределяемые модули. Модуль менеджера исключается, пока
/// `include_manager_module` не выставлен, — и только здесь: фактические записи
/// движений в нём видны всегда (слой `writes`).
fn build_mentions(
    conn: &rusqlite::Connection,
    object: &str,
    reach_files: &[String],
    include_manager_module: bool,
    heuristic: bool,
    limit: i64,
) -> rusqlite::Result<Mentions> {
    let mut sql = String::from(
        "SELECT u.object_ref, f.name, u.file_path, COUNT(*) AS hits, MIN(u.line) AS first_line \
           FROM metadata_code_usages u \
           JOIN data_links d ON d.repo = u.repo AND d.to_object = u.object_ref \
                            AND d.link_kind = 'recorder' AND d.from_object = ?1 \
           JOIN files fl ON fl.path = u.file_path \
           JOIN functions f ON f.file_id = fl.id AND u.line BETWEEN f.line_start AND f.line_end \
          WHERE u.repo = 'default'",
    );
    let mut binds: Vec<String> = vec![object.to_string()];

    if reach_files.is_empty() {
        if heuristic {
            sql.push_str(
                " AND u.file_path NOT LIKE '%/Forms/%' \
                  AND u.file_path NOT LIKE '%Переопределяемый%'",
            );
        }
    } else {
        let placeholders = vec!["?"; reach_files.len()].join(",");
        sql.push_str(&format!(" AND u.file_path IN ({})", placeholders));
        binds.extend(reach_files.iter().cloned());
    }
    if !include_manager_module {
        sql.push_str(" AND u.file_path NOT LIKE '%/Ext/ManagerModule.bsl'");
    }
    // limit — проверенное целое (clamp выше), подстановка литералом безопасна.
    sql.push_str(&format!(
        " GROUP BY u.object_ref, f.name, u.file_path \
          ORDER BY hits DESC, u.object_ref, f.name LIMIT {}",
        limit + 1
    ));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(binds), |r| {
        Ok(MentionRow {
            register: r.get(0)?,
            procedure: r.get(1)?,
            file: r.get(2)?,
            hits: r.get(3)?,
            first_line: r.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    let truncated = out.len() as i64 > limit;
    if truncated {
        out.truncate(limit as usize);
    }
    Ok(Mentions {
        rows: out,
        truncated,
    })
}

/// Объявленные регистры, которых нет ни в записях, ни в упоминаниях.
fn registers_without_code(
    declared: &[String],
    writes: &[WriteRow],
    mentions: &[MentionRow],
) -> Vec<String> {
    let mut touched: HashSet<&str> = HashSet::new();
    for w in writes {
        if let Some(r) = &w.register {
            touched.insert(r.as_str());
        }
    }
    for m in mentions {
        touched.insert(m.register.as_str());
    }
    declared
        .iter()
        .filter(|r| !touched.contains(r.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_index_core::storage::Storage;
    use tempfile::TempDir;

    const DOC: &str = "Document.ТестовыйДокумент";
    const REG_ACC: &str = "AccumulationRegister.РегНакопления";
    const REG_ACC_OTHER: &str = "AccumulationRegister.ЧужойРегистр";
    const REG_ACC_SILENT: &str = "AccumulationRegister.РегБезКода";
    const REG_BUH: &str = "AccountingRegister.РегБухгалтерии";
    const OBJ_MODULE: &str = "base/Documents/ТестовыйДокумент/Ext/ObjectModule.bsl";
    const MGR_MODULE: &str = "base/Documents/ТестовыйДокумент/Ext/ManagerModule.bsl";
    const COM_MODULE: &str = "base/CommonModules/УчетТоваров/Ext/Module.bsl";
    const FORM_MODULE: &str = "base/Documents/ТестовыйДокумент/Forms/Форма/Ext/Form/Module.bsl";

    /// Срез реальной конфигурации: записи движений в общем модуле (типовой
    /// случай — 72 % записей), запись в модуле менеджера, чтение вместо записи,
    /// запись по чужому регистру, упоминание без записи и объявленный регистр
    /// вообще без кода.
    fn fixture() -> (TempDir, Storage) {
        let tmp = TempDir::new().unwrap();
        let storage = Storage::open_file(&tmp.path().join("index.db")).unwrap();
        storage
            .apply_schema_extensions(crate::schema::SCHEMA_EXTENSIONS)
            .unwrap();
        let conn = storage.conn();

        let add_file = |path: &str| -> i64 {
            conn.execute(
                "INSERT INTO files (path, content_hash, language, lines_total) \
                 VALUES (?1, 'h', 'bsl', 100)",
                params![path],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        let f_obj = add_file(OBJ_MODULE);
        let f_mgr = add_file(MGR_MODULE);
        let f_com = add_file(COM_MODULE);
        let f_form = add_file(FORM_MODULE);

        for (file_id, name, start, end) in [
            (f_obj, "ОбработкаПроведения", 10_i64, 30_i64),
            (f_com, "СформироватьДвижения", 20, 40),
            (f_mgr, "СформироватьПроводки", 5, 15),
            (f_form, "ПриЗаписи", 1, 10),
        ] {
            conn.execute(
                "INSERT INTO functions (file_id, name, line_start, line_end) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![file_id, name, start, end],
            )
            .unwrap();
        }

        // Граф: обработчик проведения зовёт общий модуль (типовой случай) и
        // модуль менеджера — второй нужен, чтобы проверить, что слой записей
        // его не исключает, а слой упоминаний исключает.
        for (caller, callee, file) in [
            (
                format!("{}::ОбработкаПроведения", OBJ_MODULE),
                "СформироватьДвижения",
                COM_MODULE,
            ),
            (
                format!("{}::ОбработкаПроведения", OBJ_MODULE),
                "СформироватьПроводки",
                MGR_MODULE,
            ),
        ] {
            conn.execute(
                "INSERT INTO proc_call_graph \
                     (repo, caller_proc_key, callee_proc_name, callee_proc_key, call_type) \
                 VALUES ('default', ?1, ?2, ?3, 'direct')",
                params![caller, callee, format!("{}::{}", file, callee)],
            )
            .unwrap();
        }

        for (path, callee, line, caller) in [
            (
                COM_MODULE,
                "Движения.РегНакопления.Добавить",
                25_i64,
                "СформироватьДвижения",
            ),
            (
                COM_MODULE,
                "Движения.РегНакопления.Найти",
                27,
                "СформироватьДвижения",
            ),
            (
                COM_MODULE,
                "Движения.ЧужойРегистр.Добавить",
                26,
                "СформироватьДвижения",
            ),
            (COM_MODULE, "Движения.Записать", 28, "СформироватьДвижения"),
            (MGR_MODULE, "Движения.Добавить", 14, "СформироватьПроводки"),
            (
                MGR_MODULE,
                "Движения.РегБухгалтерии.Добавить",
                12,
                "СформироватьПроводки",
            ),
            (
                MGR_MODULE,
                "Движения.РегБухгалтерии.ДополнительныеСвойства.Вставить",
                13,
                "СформироватьПроводки",
            ),
            (
                FORM_MODULE,
                "Движения.РегНакопления.Добавить",
                6,
                "ПриЗаписи",
            ),
        ] {
            let file_id: i64 = conn
                .query_row("SELECT id FROM files WHERE path = ?1", params![path], |r| {
                    r.get(0)
                })
                .unwrap();
            conn.execute(
                "INSERT INTO calls (file_id, caller, callee, line) VALUES (?1, ?2, ?3, ?4)",
                params![file_id, caller, callee, line],
            )
            .unwrap();
        }

        // Декларативный состав движений: три регистра, один из них — без кода.
        for register in [REG_ACC, REG_BUH, REG_ACC_SILENT] {
            conn.execute(
                "INSERT INTO data_links (repo, from_object, from_path, to_object, link_kind) \
                 VALUES ('default', ?1, '', ?2, 'recorder')",
                params![DOC, register],
            )
            .unwrap();
        }
        // Упоминания: общий модуль (запись), модуль менеджера (шум запросов) и
        // обращение к регистру, которого нет в составе движений документа.
        for (register, path, line) in [
            (REG_ACC, COM_MODULE, 25_i64),
            (REG_BUH, MGR_MODULE, 12),
            (REG_ACC_OTHER, COM_MODULE, 26),
        ] {
            conn.execute(
                "INSERT INTO metadata_code_usages \
                     (repo, object_ref, object_ref_key, usage_kind, file_path, line) \
                 VALUES ('default', ?1, ?2, 'manager', ?3, ?4)",
                params![register, register.to_lowercase(), path, line],
            )
            .unwrap();
        }

        // Модули объекта: код проведения документа.
        for (object_name, module_type, path) in [
            ("Documents.ТестовыйДокумент", "ObjectModule", OBJ_MODULE),
            ("Documents.ТестовыйДокумент", "ManagerModule", MGR_MODULE),
        ] {
            conn.execute(
                "INSERT INTO metadata_modules \
                     (repo, full_name, object_name, module_type, object_id, property_id, code_path) \
                 VALUES ('default', ?1, ?2, ?3, 'oid', 'pid', ?4)",
                params![
                    format!("{}.{}", object_name, module_type),
                    object_name,
                    module_type,
                    path
                ],
            )
            .unwrap();
        }

        (tmp, storage)
    }

    fn declared(conn: &rusqlite::Connection) -> Vec<String> {
        declared_registers(conn, DOC).unwrap().0
    }

    fn reach_set(conn: &rusqlite::Connection) -> HashSet<String> {
        let entry = resolve_entry_point(conn, DOC, &mut Vec::new())
            .unwrap()
            .expect("обработчик проведения в фикстуре есть");
        reachable(conn, &entry, DEFAULT_DEPTH)
            .unwrap()
            .0
            .into_iter()
            .collect()
    }

    #[test]
    fn parse_movement_call_takes_writes_only() {
        assert_eq!(
            parse_movement_call("Движения.РегНакопления.Добавить"),
            Some((Some("РегНакопления"), "Добавить"))
        );
        assert_eq!(
            parse_movement_call("Движения.Хозрасчетный.Записать"),
            Some((Some("Хозрасчетный"), "Записать"))
        );
        assert_eq!(
            parse_movement_call("Движения.Записать"),
            Some((None, "Записать"))
        );
        // Добавление набора в коллекцию по имени — тоже действие по коллекции,
        // а не запись в базу: имя регистра лежит в аргументе и не индексируется.
        assert_eq!(
            parse_movement_call("Движения.Добавить"),
            Some((None, "Добавить"))
        );
        // Чтение, поиск и свойства набора записью не считаются.
        assert_eq!(parse_movement_call("Движения.РегНакопления.Найти"), None);
        assert_eq!(
            parse_movement_call("Движения.РегНакопления.Количество"),
            None
        );
        assert_eq!(
            parse_movement_call("Движения.РегБухгалтерии.ДополнительныеСвойства.Вставить"),
            None
        );
        // Не про движения — не наше.
        assert_eq!(parse_movement_call("Запрос.Выполнить"), None);
    }

    #[test]
    fn writes_are_attributed_to_procedure_and_register() {
        let (_tmp, storage) = fixture();
        let conn = storage.conn();
        let files = reach_set(conn);
        let declared = declared(conn);
        let writes =
            collect_writes(conn, &declared, WriteScope::Files(&files), CAP_WRITES).unwrap();
        let hit = writes
            .rows
            .iter()
            .find(|r| r.register.as_deref() == Some(REG_ACC))
            .expect("запись движения в общем модуле должна быть найдена");
        assert_eq!(hit.procedure, "СформироватьДвижения");
        assert_eq!(hit.file, COM_MODULE);
        assert_eq!(hit.line, 25);
        assert_eq!(hit.member, "Добавить");
        assert_eq!(hit.hits, 1);
    }

    #[test]
    fn writes_ignore_reads_and_undeclared_registers() {
        let (_tmp, storage) = fixture();
        let conn = storage.conn();
        let files = reach_set(conn);
        let declared = declared(conn);
        let writes =
            collect_writes(conn, &declared, WriteScope::Files(&files), CAP_WRITES).unwrap();
        // Чужой регистр — не состав движений этого документа.
        assert!(!writes
            .rows
            .iter()
            .any(|r| r.register.as_deref() == Some(REG_ACC_OTHER)));
        // `.Найти` — чтение, а не запись.
        assert!(writes.rows.iter().all(|r| r.member != "Найти"));
        // Свойство набора записью не считается.
        assert!(writes
            .rows
            .iter()
            .all(|r| r.file != MGR_MODULE || r.line != 13));
    }

    #[test]
    fn writes_keep_manager_module_while_mentions_drop_it() {
        let (_tmp, storage) = fixture();
        let conn = storage.conn();
        let files = reach_set(conn);
        let declared = declared(conn);

        let writes =
            collect_writes(conn, &declared, WriteScope::Files(&files), CAP_WRITES).unwrap();
        let mgr_write = writes
            .rows
            .iter()
            .find(|r| r.register.as_deref() == Some(REG_BUH))
            .expect("запись в модуле менеджера должна остаться в слое writes");
        assert_eq!(mgr_write.procedure, "СформироватьПроводки");
        assert_eq!(mgr_write.file, MGR_MODULE);

        // Слой упоминаний модуль менеджера исключает по умолчанию…
        let reach: Vec<String> = files.iter().cloned().collect();
        let mentions = build_mentions(conn, DOC, &reach, false, false, CAP_MENTIONS).unwrap();
        assert!(mentions.rows.iter().all(|r| r.file != MGR_MODULE));
        assert!(mentions.rows.iter().any(|r| r.file == COM_MODULE));

        // …а с явным флагом — включает.
        let mentions = build_mentions(conn, DOC, &reach, true, false, CAP_MENTIONS).unwrap();
        assert!(mentions.rows.iter().any(|r| r.file == MGR_MODULE));
    }

    #[test]
    fn whole_set_write_kept_only_next_to_register_writes() {
        let (_tmp, storage) = fixture();
        let conn = storage.conn();
        let files = reach_set(conn);
        let declared = declared(conn);
        let writes =
            collect_writes(conn, &declared, WriteScope::Files(&files), CAP_WRITES).unwrap();
        let whole: Vec<&WriteRow> = writes
            .rows
            .iter()
            .filter(|r| r.register.is_none())
            .collect();
        // Коллекционные вызовы видны из файлов, где пишутся регистры этого
        // документа: `Движения.Записать` — в общем модуле (запись всего набора),
        // `Движения.Добавить` — в модуле менеджера (добавление набора по имени).
        assert_eq!(whole.len(), 2);
        let flush = whole
            .iter()
            .find(|r| r.member == "Записать")
            .expect("запись всего набора");
        assert_eq!(flush.file, COM_MODULE);
        assert!(flush.register.is_none());
        let add = whole
            .iter()
            .find(|r| r.member == "Добавить")
            .expect("добавление набора в коллекцию");
        assert_eq!(add.file, MGR_MODULE);
        assert!(add.register.is_none());
    }

    #[test]
    fn form_module_writes_are_outside_call_graph_scope() {
        let (_tmp, storage) = fixture();
        let conn = storage.conn();
        let files = reach_set(conn);
        let declared = declared(conn);
        let writes =
            collect_writes(conn, &declared, WriteScope::Files(&files), CAP_WRITES).unwrap();
        // Форма не достижима от обработчика проведения — её записи в карту не идут.
        assert!(writes.rows.iter().all(|r| r.file != FORM_MODULE));
    }

    #[test]
    fn registers_without_code_lists_registers_without_writes_and_mentions() {
        let (_tmp, storage) = fixture();
        let conn = storage.conn();
        let files = reach_set(conn);
        let declared = declared(conn);
        let writes =
            collect_writes(conn, &declared, WriteScope::Files(&files), CAP_WRITES).unwrap();
        let reach: Vec<String> = files.iter().cloned().collect();
        let mentions = build_mentions(conn, DOC, &reach, true, false, CAP_MENTIONS).unwrap();
        let silent = registers_without_code(&declared, &writes.rows, &mentions.rows);
        assert_eq!(silent, vec![REG_ACC_SILENT.to_string()]);
    }

    #[test]
    fn missing_posting_handler_goes_to_heuristic_with_hint() {
        let (_tmp, storage) = fixture();
        let conn = storage.conn();
        // Убираем обработчик проведения и добавляем процедуру с «Проведен» в
        // имени: точкой входа она стать не должна — только подсказкой, иначе
        // карта выглядит точной, описывая пару файлов.
        conn.execute(
            "DELETE FROM functions WHERE name = 'ОбработкаПроведения'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO functions (file_id, name, line_start, line_end) \
             SELECT id, 'ПодготовитьПроведение', 20, 25 FROM files WHERE path = ?1",
            params![MGR_MODULE],
        )
        .unwrap();

        let mut warnings = Vec::new();
        let entry = resolve_entry_point(conn, DOC, &mut warnings).unwrap();
        assert!(
            entry.is_none(),
            "точки входа быть не должно — уходим в эвристику"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("ПодготовитьПроведение") && w.contains("не найден")),
            "ожидалась подсказка о похожей процедуре: {:?}",
            warnings
        );
    }
}

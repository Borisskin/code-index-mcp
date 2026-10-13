// Структурные инструменты: контекст вызовов и скелет кода.
//
// Три инструмента ступени B:
//   * `query_call_sites` — вызовы с фильтром по контексту (в цикле, в
//     `Попытке`, глубина вложенности);
//   * `get_node_structure` — скелет процедуры: циклы, `Попытка`, ветки, с
//     ограничением глубины;
//   * `get_line_context` — что находится на строке: процедура, блоки, признаки.
//
// **Хранимого артефакта нет — и это осознанное решение** (см.
// `docs/plan-asg-and-entry-points.md`, ступень B). Замер показал, что разбор
// модуля стоит 1,4–2,4 мс: инструменту на вызов нужно 20–50 модулей, то есть
// 0,15–0,4 с. Хранимый блоб сэкономил бы эти миллисекунды, а стоил бы роста
// базы (+0,65 % от объёма исходников по замеру на шести базах), разового
// пересбора на каждой базе и поднятия номера версии данных.
//
// Отсюда и главное ограничение: инструменты работают быстро, пока запрос
// **сужен** — по вызываемому, по вызывающему или по файлу. Широкий запрос
// («все вызовы в циклах по всей конфигурации») заставил бы разобрать тысячи
// модулей, поэтому у `query_call_sites` сужение обязательно, а число
// разобранных модулей ограничено и возвращается в ответе.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use code_index_core::extension::{IndexTool, ToolContext};
use code_index_core::parser::bsl::{parse_structure, BslBlockKind, BslStructure};
use rusqlite::params;
use serde_json::{json, Value};

/// Сколько модулей инструмент готов разобрать за один вызов. Потолок честно
/// возвращается в ответе (`files_scanned`, `files_limit`) — молча урезать
/// выборку нельзя, иначе «в цикле ничего нет» будет означать «не дочитали».
const MAX_FILES_PER_CALL: usize = 50;

/// Потолок строк выдачи и его предел.
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

/// Кэш разбора в пределах одного вызова: один и тот же модуль разбирается один
/// раз, даже если инструмент обращается к нему несколько раз.
#[derive(Default)]
struct StructureCache {
    map: HashMap<String, Arc<BslStructure>>,
}

impl StructureCache {
    fn get(&mut self, path: &str, source: &str) -> Arc<BslStructure> {
        if let Some(s) = self.map.get(path) {
            return Arc::clone(s);
        }
        let parsed = Arc::new(parse_structure(source));
        self.map.insert(path.to_string(), Arc::clone(&parsed));
        parsed
    }
}

/// Прочитать модуль: сперва из базы (там он лежит сжатым), при отсутствии —
/// с диска. Возвращает текст и признак источника.
fn read_module(
    storage: &code_index_core::storage::Storage,
    root: Option<&std::path::Path>,
    path: &str,
) -> Option<(String, &'static str)> {
    let file_id: Option<i64> = storage
        .conn()
        .query_row("SELECT id FROM files WHERE path = ?1", params![path], |r| {
            r.get::<_, i64>(0)
        })
        .ok();
    if let Some(id) = file_id {
        if let Ok(Some((Some(content), false))) = storage.read_file_content(id) {
            return Some((content, "db"));
        }
    }
    let root = root?;
    let full = root.join(path);
    std::fs::read_to_string(full).ok().map(|c| (c, "disk"))
}

/// Строки файла, в которых встречается процедура: границы из таблицы
/// `functions` (там же, где их берут остальные инструменты).
fn procedure_range(
    storage: &code_index_core::storage::Storage,
    path: &str,
    name: &str,
) -> Option<(usize, usize)> {
    storage
        .conn()
        .query_row(
            "SELECT fn.line_start, fn.line_end FROM functions fn \
             JOIN files f ON f.id = fn.file_id \
             WHERE f.path = ?1 AND fn.name = ?2 \
             ORDER BY fn.line_start LIMIT 1",
            params![path, name],
            |r| Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize)),
        )
        .ok()
}

/// Имена процедур, определённых в файле (для ответа «в какой процедуре строка»).
fn procedures_of(
    storage: &code_index_core::storage::Storage,
    path: &str,
) -> Vec<(String, usize, usize)> {
    let conn = storage.conn();
    let Ok(mut stmt) = conn.prepare(
        "SELECT fn.name, fn.line_start, fn.line_end FROM functions fn \
         JOIN files f ON f.id = fn.file_id WHERE f.path = ?1 ORDER BY fn.line_start",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map(params![path], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)? as usize,
            r.get::<_, i64>(2)? as usize,
        ))
    });
    match rows {
        Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Файлы-кандидаты по вызываемому: где вообще есть рёбра к этому имени.
/// Именно здесь запрос сужается — иначе пришлось бы разбирать всю базу.
///
/// Путь берём соединением с `files`, а не «отрезанием» ключа вызывателя: у
/// платформенных слоёв ключ синтетический (`form::…`, `event::…`), и
/// отрезанная часть — не путь, а мусор. Такой «файл» не прочитался бы и просто
/// съел бы место в потолке разбора, из-за чего часть настоящих модулей не
/// проверялась бы.
fn files_calling(
    storage: &code_index_core::storage::Storage,
    callee: &str,
    limit: usize,
) -> Vec<String> {
    let conn = storage.conn();
    let Ok(mut stmt) = conn.prepare(
        "SELECT DISTINCT f.path FROM proc_call_graph g \
         JOIN files f ON f.path = substr(g.caller_proc_key, 1, instr(g.caller_proc_key, '::') - 1) \
         WHERE g.callee_proc_name = ?1 AND g.caller_proc_key LIKE '%::%' \
         ORDER BY f.path LIMIT ?2",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map(params![callee, limit as i64], |r| r.get::<_, String>(0));
    match rows {
        Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Файлы, где определена процедура с этим именем.
fn files_defining(
    storage: &code_index_core::storage::Storage,
    name: &str,
    limit: usize,
) -> Vec<String> {
    let conn = storage.conn();
    let Ok(mut stmt) = conn.prepare(
        "SELECT DISTINCT f.path FROM functions fn JOIN files f ON f.id = fn.file_id \
         WHERE fn.name = ?1 ORDER BY f.path LIMIT ?2",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map(params![name, limit as i64], |r| r.get::<_, String>(0));
    match rows {
        Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

fn block_kind_name(kind: BslBlockKind) -> &'static str {
    match kind {
        BslBlockKind::Loop => "loop",
        BslBlockKind::Try => "try",
        BslBlockKind::If => "if",
    }
}

fn limit_of(args: &Value) -> usize {
    args.get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT)
}

// ── query_call_sites ─────────────────────────────────────────────────────

pub struct QueryCallSitesTool;

impl IndexTool for QueryCallSitesTool {
    fn name(&self) -> &str {
        "query_call_sites"
    }

    fn applicable_languages(&self) -> Option<&'static [&'static str]> {
        Some(&["bsl"])
    }

    fn description(&self) -> &str {
        "Вызовы процедур с фильтром по МЕСТУ вызова: в цикле (in_loop=true), в \
         Попытке/Исключении (in_exception=true), не глубже min_depth, в конкретной \
         процедуре или файле. Отвечает на вопросы «что крутится в цикле» и «что \
         делается в обработчике исключения», которые get_callees не различает. \
         СУЖЕНИЕ ОБЯЗАТЕЛЬНО: укажите хотя бы одно из callee (имя вызываемого), \
         caller (имя процедуры) или path (путь модуля) — без него инструмент \
         вернёт ошибку, потому что широкий запрос потребовал бы разобрать всю \
         конфигурацию. Разбирается не больше 50 модулей за вызов; сколько \
         разобрано и сколько было кандидатов — в полях files_scanned и \
         files_limit. Выдача ограничена limit (по умолчанию 50, максимум 500), \
         при обрезке — truncated=true. For BSL/1C repositories only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Алиас репозитория" },
                "callee": {
                    "type": "string",
                    "description": "Имя вызываемой процедуры (как в коде; для квалифицированного вызова — Модуль.Метод)"
                },
                "caller": {
                    "type": "string",
                    "description": "Имя процедуры-вызывателя: фильтр по месту, откуда вызов сделан"
                },
                "path": {
                    "type": "string",
                    "description": "Путь модуля: сузить поиск одним файлом"
                },
                "in_loop": { "type": "boolean", "description": "Только вызовы внутри цикла" },
                "in_exception": { "type": "boolean", "description": "Только вызовы внутри Попытки" },
                "min_depth": {
                    "type": "integer",
                    "description": "Минимальная глубина вложенности блоков (0 — любой уровень)"
                },
                "limit": { "type": "integer", "description": "Потолок строк выдачи (по умолчанию 50, максимум 500)" }
            },
            "required": ["repo"]
        })
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Value> + Send + 'a>> {
        Box::pin(async move {
            let callee = args.get("callee").and_then(|v| v.as_str());
            let caller = args.get("caller").and_then(|v| v.as_str());
            let path = args.get("path").and_then(|v| v.as_str());
            if callee.is_none() && caller.is_none() && path.is_none() {
                return json!({
                    "error": "нужно сужение: укажите callee, caller или path",
                    "hint": "Инструмент не разбирает всю конфигурацию: без сужения он вернул бы \
                             тысячи модулей и считался бы минутами. Например: \
                             query_call_sites(callee=\"РассчитатьСебестоимость\", in_loop=true) — \
                             где эта процедура вызывается в циклах."
                });
            }
            let want_loop = args.get("in_loop").and_then(|v| v.as_bool());
            let want_exc = args.get("in_exception").and_then(|v| v.as_bool());
            let min_depth = args.get("min_depth").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
            let limit = limit_of(&args);

            let storage = match ctx.storage.get().await {
                Ok(s) => s,
                Err(e) => return json!({ "error": format!("хранилище недоступно: {e}") }),
            };

            // Кандидаты: файл, процедура-вызыватель или вызываемый.
            let candidates: Vec<String> = match (path, caller, callee) {
                (Some(p), _, _) => vec![p.to_string()],
                (None, Some(c), _) => files_defining(&storage, c, MAX_FILES_PER_CALL),
                (None, None, Some(callee)) => files_calling(&storage, callee, MAX_FILES_PER_CALL),
                _ => Vec::new(),
            };

            let mut rows: Vec<Value> = Vec::new();
            let mut cache = StructureCache::default();
            let mut scanned = 0usize;
            let mut truncated = false;
            let mut sources: HashMap<&'static str, usize> = HashMap::new();

            'outer: for file in candidates.iter().take(MAX_FILES_PER_CALL) {
                let Some((source, from)) = read_module(&storage, ctx.root_path, file) else {
                    continue;
                };
                *sources.entry(from).or_insert(0) += 1;
                scanned += 1;
                let structure = cache.get(file, &source);
                for call in &structure.calls {
                    if rows.len() >= limit {
                        truncated = true;
                        break 'outer;
                    }
                    if let Some(c) = callee {
                        if call.callee != c {
                            continue;
                        }
                    }
                    if let Some(c) = caller {
                        if call.caller != c {
                            continue;
                        }
                    }
                    if let Some(want) = want_loop {
                        if call.in_loop != want {
                            continue;
                        }
                    }
                    if let Some(want) = want_exc {
                        if call.in_exception != want {
                            continue;
                        }
                    }
                    if call.depth < min_depth {
                        continue;
                    }
                    rows.push(json!({
                        "path": file,
                        "caller": call.caller,
                        "callee": call.callee,
                        "line": call.line,
                        "in_loop": call.in_loop,
                        "in_exception": call.in_exception,
                        "depth": call.depth,
                    }));
                }
            }

            let mut out = json!({
                "calls": rows,
                "count": rows.len(),
                "files_candidates": candidates.len(),
                "files_scanned": scanned,
                "files_limit": MAX_FILES_PER_CALL,
                "limit": limit,
            });
            if truncated {
                out["truncated"] = json!(true);
            }
            if candidates.len() > MAX_FILES_PER_CALL {
                out["candidates_truncated"] = json!(true);
                out["hint"] = json!(
                    "Кандидатов больше потолка разбора: сузьте запрос (например, добавьте path \
                     или caller) — иначе часть модулей не проверялась."
                );
            }
            if let Some(db) = sources.get("db") {
                out["sources"] =
                    json!({ "db": db, "disk": sources.get("disk").copied().unwrap_or(0) });
            }
            out
        })
    }
}

// ── get_node_structure ───────────────────────────────────────────────────

pub struct GetNodeStructureTool;

impl IndexTool for GetNodeStructureTool {
    fn name(&self) -> &str {
        "get_node_structure"
    }

    fn applicable_languages(&self) -> Option<&'static [&'static str]> {
        Some(&["bsl"])
    }

    fn description(&self) -> &str {
        "Скелет процедуры 1С: циклы, Попытки и ветки с номерами строк и глубиной \
         вложенности — без чтения тела целиком. Нужен, когда надо понять устройство \
         процедуры на 300–2000 строк: где циклы, где обработка исключений, сколько \
         веток. Параметры: name — имя процедуры (обязательно, если не задан path+line), \
         path — сузить до конкретного модуля, max_depth — предел глубины (по умолчанию \
         3; при обрезке в ответе depth_truncated=true), limit — потолок блоков \
         (по умолчанию 50, максимум 500). В ответе также переменные: `variables` — \
         объявленные в самой процедуре (локальные, с номерами строк), \
         `module_variables` — модульные. Модуль разбирается на месте (1,4–2,4 мс), \
         хранимого артефакта нет. For BSL/1C repositories only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Алиас репозитория" },
                "name": { "type": "string", "description": "Имя процедуры или функции" },
                "path": { "type": "string", "description": "Путь модуля, если имя неоднозначно" },
                "max_depth": { "type": "integer", "description": "Предел глубины блоков (по умолчанию 3)" },
                "limit": { "type": "integer", "description": "Потолок числа блоков в ответе (по умолчанию 50)" }
            },
            "required": ["repo", "name"]
        })
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Value> + Send + 'a>> {
        Box::pin(async move {
            let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
                return json!({ "error": "нужно имя процедуры: name" });
            };
            let explicit_path = args.get("path").and_then(|v| v.as_str());
            let max_depth = args
                .get("max_depth")
                .and_then(|v| v.as_u64())
                .unwrap_or(3)
                .clamp(1, 32) as u8;
            let limit = limit_of(&args);

            let storage = match ctx.storage.get().await {
                Ok(s) => s,
                Err(e) => return json!({ "error": format!("хранилище недоступно: {e}") }),
            };

            let files = match explicit_path {
                Some(p) => vec![p.to_string()],
                None => files_defining(&storage, name, MAX_FILES_PER_CALL),
            };
            let mut found: Option<(String, usize, usize, Arc<BslStructure>, &'static str)> = None;
            for file in &files {
                let Some((source, from)) = read_module(&storage, ctx.root_path, file) else {
                    continue;
                };
                let Some((start, end)) = procedure_range(&storage, file, name) else {
                    continue;
                };
                let structure = parse_structure(&source);
                found = Some((file.clone(), start, end, Arc::new(structure), from));
                break;
            }
            let Some((path, start, end, structure, from)) = found else {
                return json!({
                    "error": format!("процедура «{name}» не найдена в индексе"),
                    "files_checked": files.len(),
                });
            };

            let mut blocks: Vec<Value> = Vec::new();
            let mut depth_truncated = false;
            for b in structure
                .blocks
                .iter()
                .filter(|b| b.line_start >= start && b.line_end <= end)
            {
                if b.depth >= max_depth {
                    depth_truncated = true;
                    continue;
                }
                if blocks.len() >= limit {
                    break;
                }
                blocks.push(json!({
                    "kind": block_kind_name(b.kind),
                    "line_start": b.line_start,
                    "line_end": b.line_end,
                    "depth": b.depth,
                }));
            }

            // Переменные: объявленные в самой процедуре отдельно от модульных —
            // локальные ядро при индексации не собирает, и до сих пор их
            // приходилось искать чтением модуля.
            let variables: Vec<Value> = structure
                .variables
                .iter()
                .filter(|v| v.caller == name)
                .take(limit)
                .map(|v| json!({ "name": v.name, "line": v.line }))
                .collect();
            let module_variables: Vec<Value> = structure
                .variables
                .iter()
                .filter(|v| v.caller == "<module>")
                .take(limit)
                .map(|v| json!({ "name": v.name, "line": v.line }))
                .collect();

            let mut out = json!({
                "procedure": name,
                "path": path,
                "line_start": start,
                "line_end": end,
                "lines": end.saturating_sub(start) + 1,
                "source": from,
                "max_depth": max_depth,
                "blocks": blocks,
                "blocks_count": blocks.len(),
                "variables": variables,
                "variables_count": structure
                    .variables
                    .iter()
                    .filter(|v| v.caller == name)
                    .count(),
                "module_variables": module_variables,
                "calls_count": structure
                    .calls
                    .iter()
                    .filter(|c| c.line >= start && c.line <= end)
                    .count(),
            });
            if depth_truncated {
                out["depth_truncated"] = json!(true);
                out["hint"] = json!(
                    "Блоки глубже max_depth не показаны: поднимите max_depth, если нужен \
                     весь скелет целиком."
                );
            }
            out
        })
    }
}

// ── get_line_context ─────────────────────────────────────────────────────

pub struct GetLineContextTool;

impl IndexTool for GetLineContextTool {
    fn name(&self) -> &str {
        "get_line_context"
    }

    fn applicable_languages(&self) -> Option<&'static [&'static str]> {
        Some(&["bsl"])
    }

    fn description(&self) -> &str {
        "Что находится на строке модуля 1С: процедура, в которую она попадает, все \
         содержащие её блоки (цикл, Попытка, ветка) с номерами строк и глубиной, и \
         признаки in_loop / in_exception. Нужен, когда строка известна (из ответа \
         другого инструмента или из текста ошибки), а понять её место в коде надо \
         без чтения модуля. Параметры: path — путь модуля, line — номер строки \
         (от 1). Модуль разбирается на месте. For BSL/1C repositories only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Алиас репозитория" },
                "path": { "type": "string", "description": "Путь модуля" },
                "line": { "type": "integer", "description": "Номер строки, от 1" }
            },
            "required": ["repo", "path", "line"]
        })
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Value> + Send + 'a>> {
        Box::pin(async move {
            let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
                return json!({ "error": "нужен путь модуля: path" });
            };
            let Some(line) = args.get("line").and_then(|v| v.as_u64()) else {
                return json!({ "error": "нужен номер строки: line (от 1)" });
            };
            let line = line as usize;

            let storage = match ctx.storage.get().await {
                Ok(s) => s,
                Err(e) => return json!({ "error": format!("хранилище недоступно: {e}") }),
            };
            let Some((source, from)) = read_module(&storage, ctx.root_path, path) else {
                return json!({ "error": format!("модуль «{path}» не найден ни в базе, ни на диске") });
            };
            let structure = parse_structure(&source);

            let procedure = procedures_of(&storage, path)
                .into_iter()
                .find(|(_, start, end)| *start <= line && line <= *end)
                .map(|(name, start, end)| json!({ "name": name, "line_start": start, "line_end": end }));

            let (in_loop, in_exception, depth) = structure.context_at(line);
            let blocks: Vec<Value> = structure
                .blocks
                .iter()
                .filter(|b| b.line_start <= line && line <= b.line_end)
                .map(|b| {
                    json!({
                        "kind": block_kind_name(b.kind),
                        "line_start": b.line_start,
                        "line_end": b.line_end,
                        "depth": b.depth,
                    })
                })
                .collect();

            json!({
                "path": path,
                "line": line,
                "source": from,
                "procedure": procedure,
                "in_loop": in_loop,
                "in_exception": in_exception,
                "depth": depth,
                "blocks": blocks,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_index_core::storage::Storage;
    use tempfile::TempDir;

    /// Хранилище с одним модулем, одной процедурой и одним ребром графа.
    fn storage_with_file(tmp: &TempDir, path: &str) -> Storage {
        let storage = Storage::open_file(&tmp.path().join("index.db")).unwrap();
        storage
            .apply_schema_extensions(crate::schema::SCHEMA_EXTENSIONS)
            .unwrap();
        let content = "Процедура Тест()\n    Вызываемая();\nКонецПроцедуры";
        let conn = storage.conn();
        conn.execute(
            "INSERT INTO files (path, content_hash, language) VALUES (?1, 'h', 'bsl')",
            params![path],
        )
        .unwrap();
        let file_id: i64 = conn
            .query_row("SELECT id FROM files WHERE path = ?1", params![path], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute(
            "INSERT INTO functions (file_id, name, line_start, line_end) VALUES (?1, 'Тест', 1, 3)",
            params![file_id],
        )
        .unwrap();
        // Настоящее ребро: ключ вызывателя — путь плюс процедура.
        conn.execute(
            "INSERT INTO proc_call_graph (repo, caller_proc_key, callee_proc_name, call_type) \
             VALUES ('default', ?1, 'Вызываемая', 'direct')",
            params![format!("{path}::Тест")],
        )
        .unwrap();
        // Синтетическое ребро платформенного слоя: путь из него не выводится.
        conn.execute(
            "INSERT INTO proc_call_graph (repo, caller_proc_key, callee_proc_name, call_type) \
             VALUES ('default', 'form::Documents.X::Форма::ПриОткрытии', 'Вызываемая', 'form_event')",
            [],
        )
        .unwrap();
        storage
            .upsert_file_content(file_id, content, 5 * 1024 * 1024)
            .unwrap();
        storage
    }

    /// Сужение по вызываемому не должно подставлять синтетические ключи как пути.
    #[test]
    fn files_calling_skips_synthetic_keys() {
        let tmp = TempDir::new().unwrap();
        let storage = storage_with_file(&tmp, "base/Documents/X/Ext/ObjectModule.bsl");
        let files = files_calling(&storage, "Вызываемая", 50);
        assert_eq!(
            files,
            vec!["base/Documents/X/Ext/ObjectModule.bsl".to_string()],
            "в кандидаты обязан попасть только настоящий модуль"
        );
    }

    #[test]
    fn defining_and_range_are_found() {
        let tmp = TempDir::new().unwrap();
        let path = "base/Documents/X/Ext/ObjectModule.bsl";
        let storage = storage_with_file(&tmp, path);
        assert_eq!(files_defining(&storage, "Тест", 50), vec![path.to_string()]);
        assert_eq!(procedure_range(&storage, path, "Тест"), Some((1, 3)));
        assert_eq!(procedure_range(&storage, path, "НетТакой"), None);
    }

    /// Содержимое берётся из базы — и именно оно разбирается инструментами.
    #[test]
    fn module_is_read_from_db_and_parsed() {
        let tmp = TempDir::new().unwrap();
        let path = "base/Documents/X/Ext/ObjectModule.bsl";
        let storage = storage_with_file(&tmp, path);
        let (content, from) = read_module(&storage, None, path).expect("модуль из базы");
        assert_eq!(from, "db");
        let structure = parse_structure(&content);
        assert_eq!(structure.calls.len(), 1);
        assert_eq!(structure.calls[0].callee, "Вызываемая");
        assert_eq!(structure.calls[0].caller, "Тест");
        assert!(!structure.calls[0].in_loop);
    }
}

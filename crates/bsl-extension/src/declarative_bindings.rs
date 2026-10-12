//! Декларативные привязки процедуры, которых нет и не может быть в графе
//! вызовов кода.
//!
//! Три вида:
//!
//! * **форма** — `DeclarativeCallers` уже отвечает по формам (см.
//!   [`crate::form_bindings`]): процедура объявлена в модуле формы, а зовёт её
//!   платформа по записи в описании формы;
//! * **подписка на событие** — процедура общего модуля вызывается платформой
//!   при записи объектов-источников; в коде вызова нет;
//! * **перехват расширением** — процедура расширения подменяет или дополняет
//!   процедуру основной конфигурации.
//!
//! До этого модуля `get_callers` знал только первый вид: по обработчику
//! подписки инструмент отвечал «вызывающих нет», и это читалось как «процедура
//! нигде не используется». Ответ по подписке точнее, чем по форме: сама строка
//! подписки называет и модуль, и процедуру, поэтому неоднозначности по имени
//! здесь не возникает.

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use code_index_core::storage::Storage;

use crate::index_extras::REPO_DEFAULT;

/// Сколько записей отдаём по одному виду привязки. Больше — уже не ответ, а
/// свалка: на одно событие подписок бывает несколько.
const MAX_PER_KIND: usize = 5;

/// Все декларативные привязки процедуры `function_name`: формы, подписки,
/// перехваты расширений. Готовые записи для ответа `get_callers`.
pub fn declarative_bindings(storage: &Storage, function_name: &str) -> Vec<Value> {
    if function_name.is_empty() {
        return Vec::new();
    }
    let conn = storage.conn();
    let mut out = crate::form_bindings::form_bindings(storage, function_name);
    out.extend(subscription_bindings(conn, function_name));
    out.extend(override_bindings(conn, function_name));
    out.extend(convention_bindings(conn, function_name));
    out
}

/// Сколько определений имени просматриваем, прежде чем отдать примеры.
///
/// Ограничение нужно, чтобы горячее имя (`ПередЗаписью` носят сотни процедур)
/// не тянуло весь список; но слишком узкое оставляло бы без примеров те виды
/// модулей, что идут в списке путей позже, — а ответ «платформа зовёт по
/// соглашению» без единого примера бесполезен.
const CONVENTION_CANDIDATES: i64 = 200;

/// Обработчики, которые платформа зовёт по соглашению об имени, — их привязки
/// не записаны нигде: ни в описании формы, ни в подписке.
///
/// Главный случай — `ОбработкаПроведения` документа: `get_callers` по ней
/// отвечал «вызывающих нет», хотя зовёт её платформа при каждом проведении.
/// Описание берётся из [`crate::platform_conventions`], вид модуля — из пути
/// файла: одно и то же имя в модуле объекта и в общем модуле значит разное.
fn convention_bindings(conn: &Connection, function_name: &str) -> Vec<Value> {
    // Сколько всего определений носит это имя: без числа нельзя честно сказать,
    // показаны все привязки или только часть.
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM functions WHERE name = ?1",
            params![function_name],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if total == 0 {
        return Vec::new();
    }

    let sql = "SELECT fl.path FROM functions fn JOIN files fl ON fl.id = fn.file_id \
               WHERE fn.name = ?1 ORDER BY fl.path LIMIT ?2";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(params![function_name, CONVENTION_CANDIDATES], |r| {
        r.get::<_, String>(0)
    }) else {
        return Vec::new();
    };
    let mut out: Vec<Value> = Vec::new();
    let mut matched = 0usize;
    for path in rows.flatten() {
        if out.len() >= MAX_PER_KIND {
            matched += 1;
            continue;
        }
        if let Some(desc) = crate::platform_conventions::convention(&path, function_name) {
            matched += 1;
            out.push(json!({
                "caller": format!("Платформа вызывает по соглашению: {desc}"),
                "kind": "platform_convention",
                "callee": function_name,
                "path": path,
                "note": "Вызова в коде нет и быть не может — привязка держится на имени процедуры и виде модуля.",
            }));
        }
    }
    if matched > out.len() && !out.is_empty() {
        out.push(json!({
            "kind": "platform_convention",
            "caller": format!(
                "Обработчиков платформы с этим именем больше: показаны первые {}",
                out.len()
            ),
            "hint": format!(
                "Всего определений имени — {total}; спросите про конкретный модуль (path_glob), чтобы увидеть нужный.",
            ),
        }));
    }
    out
}

/// Подписки на события, обработчиком которых назначена эта процедура.
///
/// Модуль-обработчик берётся из самой подписки, поэтому привязка точная: имя
/// процедуры в другом модуле не даёт ложного срабатывания.
fn subscription_bindings(conn: &Connection, function_name: &str) -> Vec<Value> {
    let sql = "SELECT s.event, s.handler_module, s.sources_json \
               FROM event_subscriptions s \
               WHERE s.repo = ?1 AND s.handler_proc = ?2 \
               ORDER BY s.event, s.handler_module";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(params![REPO_DEFAULT, function_name], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    }) else {
        return Vec::new();
    };
    let mut out: Vec<Value> = Vec::new();
    for (event, handler_module, sources_json) in rows.flatten() {
        if out.len() >= MAX_PER_KIND {
            out.push(json!({
                "kind": "subscription",
                "caller": format!(
                    "Подписок на процедуру «{}» больше {} — показаны первые",
                    function_name, MAX_PER_KIND
                ),
                "hint": "Полный список — в таблице event_subscriptions (bsl_sql)."
            }));
            break;
        }
        // Модуль обработчика может быть записан и как `CommonModule.Имя`, и как
        // голое `Имя`; в `metadata_modules` общий модуль значится во
        // множественном числе.
        let bare = handler_module
            .strip_prefix("CommonModule.")
            .unwrap_or(&handler_module);
        let candidates = [format!("CommonModules.{bare}"), handler_module.clone()];
        let mut path: Option<String> = None;
        let mut confirmed = false;
        for cand in &candidates {
            let found: Option<String> = conn
                .query_row(
                    "SELECT mm.code_path FROM metadata_modules mm \
                     WHERE mm.repo = ?1 AND mm.module_type = 'Module' AND mm.object_name = ?2 \
                       AND mm.code_path IS NOT NULL LIMIT 1",
                    params![REPO_DEFAULT, cand],
                    |r| r.get(0),
                )
                .ok();
            let Some(p) = found else { continue };
            let has_proc: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM functions fn JOIN files fl ON fl.id = fn.file_id \
                     WHERE fl.path = ?1 AND fn.name = ?2",
                    params![&p, function_name],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            path = Some(p);
            confirmed = has_proc > 0;
            if confirmed {
                break;
            }
        }
        let sources: Value = sources_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(Value::Null);
        let mut rec = json!({
            "caller": format!("Подписка на событие «{event}» (платформа вызывает процедуру сама)"),
            "kind": "subscription",
            "callee": function_name,
            "event": event,
            "module": handler_module,
            "sources": sources,
        });
        if let Some(p) = path {
            rec["path"] = json!(p);
            if !confirmed {
                rec["note"] = json!(
                    "Процедура с таким именем в модуле-обработчике не найдена — \
                     проверьте имя в подписке (возможно, модуль не в выгрузке)."
                );
            }
        } else {
            rec["note"] = json!("Модуль-обработчик не найден в индексе.");
        }
        out.push(rec);
    }
    out
}

/// Перехваты расширением — в обе стороны.
///
/// Спрашивают и про процедуру расширения («что она перехватывает»), и про
/// процедуру основной конфигурации («не перехвачена ли она»). Второй вопрос
/// важнее при правке: перехваченную процедуру недостаточно изменить в основной
/// конфигурации — расширение подменит её исполнение.
fn override_bindings(conn: &Connection, function_name: &str) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();

    // Сторона 1: сама процедура — перехватчик.
    if let Ok(mut stmt) = conn.prepare(
        "SELECT f.override_target, f.override_type, fl.path \
         FROM functions f JOIN files fl ON fl.id = f.file_id \
         WHERE f.name = ?1 AND f.override_type IS NOT NULL \
         ORDER BY fl.path LIMIT ?2",
    ) {
        if let Ok(rows) = stmt.query_map(params![function_name, MAX_PER_KIND as i64], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
            ))
        }) {
            for (target, otype, path) in rows.flatten() {
                out.push(json!({
                    "caller": format!(
                        "Перехват расширением: платформа зовёт базовую процедуру «{}», \
                         исполняется эта",
                        target.clone().unwrap_or_default()
                    ),
                    "kind": "extension_override",
                    "callee": function_name,
                    "override_target": target,
                    "override_type": otype,
                    "path": path,
                }));
            }
        }
    }

    // Сторона 2: процедуру основной конфигурации перехватывают.
    if let Ok(mut stmt) = conn.prepare(
        "SELECT f.name, f.override_type, fl.path \
         FROM functions f JOIN files fl ON fl.id = f.file_id \
         WHERE f.override_target = ?1 AND f.override_type IS NOT NULL \
         ORDER BY fl.path LIMIT ?2",
    ) {
        if let Ok(rows) = stmt.query_map(params![function_name, MAX_PER_KIND as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
            ))
        }) {
            for (ext_proc, otype, path) in rows.flatten() {
                out.push(json!({
                    "caller": format!(
                        "Процедура перехвачена расширением: «{ext_proc}» ({})",
                        otype.clone().unwrap_or_else(|| "вид не указан".into())
                    ),
                    "kind": "extension_override_target",
                    "callee": function_name,
                    "extension_proc": ext_proc,
                    "override_type": otype,
                    "path": path,
                }));
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_index_core::storage::Storage;
    use tempfile::TempDir;

    fn fresh_storage(tmp: &TempDir) -> Storage {
        let db_path = tmp.path().join("index.db");
        let storage = Storage::open_file(&db_path).unwrap();
        storage
            .apply_schema_extensions(crate::schema::SCHEMA_EXTENSIONS)
            .unwrap();
        storage
    }

    #[test]
    fn subscription_and_override_bindings_are_reported() {
        let tmp = TempDir::new().unwrap();
        let storage = fresh_storage(&tmp);
        let conn = storage.conn();
        conn.execute_batch(
            "INSERT INTO files (id, path, content_hash, language, lines_total) VALUES
                 (1, 'CommonModules/М/Ext/Module.bsl', 'h1', 'bsl', 3),
                 (2, 'extensions/Расш/CommonModules/Р/Ext/Module.bsl', 'h2', 'bsl', 3);
             INSERT INTO functions (file_id, name, line_start, line_end, override_type, override_target) VALUES
                 (1, 'П', 1, 3, NULL, NULL),
                 (2, 'Расш_П', 1, 3, 'Вместо', 'П');
             INSERT INTO metadata_modules
                 (repo, full_name, object_name, module_type, object_id, property_id, code_path) VALUES
                 ('default', 'CommonModules.М.Module', 'CommonModules.М', 'Module', 'u1', 'p1',
                  'CommonModules/М/Ext/Module.bsl');
             INSERT INTO event_subscriptions (repo, name, event, handler_module, handler_proc, sources_json) VALUES
                 ('default', 'Sub', 'ПриЗаписи', 'CommonModule.М', 'П', '[\"cfg:DocumentRef.X\"]');",
        )
        .unwrap();

        // Спрашиваем про обработчик подписки: он же — перехваченная процедура.
        let recs = declarative_bindings(&storage, "П");
        let sub = recs
            .iter()
            .find(|r| r["kind"] == "subscription")
            .expect("подписка обязана найтись по имени обработчика");
        assert_eq!(sub["event"], "ПриЗаписи");
        assert_eq!(sub["module"], "CommonModule.М");
        assert_eq!(sub["path"], "CommonModules/М/Ext/Module.bsl");
        assert_eq!(sub["sources"][0], "cfg:DocumentRef.X");
        assert!(
            recs.iter()
                .any(|r| r["kind"] == "extension_override_target"),
            "перехват процедуры расширением обязан быть виден"
        );

        // Спрашиваем про процедуру расширения: что она перехватывает.
        let ext = declarative_bindings(&storage, "Расш_П");
        let ovr = ext
            .iter()
            .find(|r| r["kind"] == "extension_override")
            .expect("перехватчик обязан находиться по своему имени");
        assert_eq!(ovr["override_target"], "П");
        assert_eq!(ovr["override_type"], "Вместо");
    }

    #[test]
    fn unknown_procedure_has_no_bindings() {
        let tmp = TempDir::new().unwrap();
        let storage = fresh_storage(&tmp);
        assert!(declarative_bindings(&storage, "НетТакой").is_empty());
        assert!(declarative_bindings(&storage, "").is_empty());
    }

    #[test]
    fn platform_convention_handler_is_reported() {
        let tmp = TempDir::new().unwrap();
        let storage = fresh_storage(&tmp);
        let conn = storage.conn();
        conn.execute_batch(
            "INSERT INTO files (id, path, content_hash, language, lines_total) VALUES
                 (1, 'base/Documents/Реализация/Ext/ObjectModule.bsl', 'h3', 'bsl', 3);
             INSERT INTO functions (file_id, name, line_start, line_end) VALUES
                 (1, 'ОбработкаПроведения', 1, 3);",
        )
        .unwrap();

        let recs = declarative_bindings(&storage, "ОбработкаПроведения");
        let conv = recs
            .iter()
            .find(|r| r["kind"] == "platform_convention")
            .expect("обработчик проведения обязан опознаваться как точка входа платформы");
        assert!(conv["caller"]
            .as_str()
            .unwrap_or_default()
            .contains("проведение"));
        assert_eq!(
            conv["path"],
            "base/Documents/Реализация/Ext/ObjectModule.bsl"
        );
    }
}

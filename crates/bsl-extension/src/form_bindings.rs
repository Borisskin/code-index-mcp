// Привязки обработчиков форм — «вызыватели», которых нет в графе вызовов кода.
//
// Процедура-обработчик объявлена в модуле формы, но в коде её никто не
// вызывает: платформа выполняет её сама по записи в описании формы
// (`<Event name="BeforeWriteAtServer">ПередЗаписьюНаСервере1</Event>`).
// Поэтому `get_callers` по такой процедуре честно отдавал ноль рёбер, а
// читалось это как «нигде не используется» — и дальше как «индекс отстал».
//
// Связь ищется ПО ФАЙЛУ МОДУЛЯ, а не по имени обработчика: имена вроде
// `ПриОткрытии` встречаются в сотнях форм, и поиск по имени в `handlers_json`
// дал бы чужие формы (плюс полный скан таблицы на каждый вызов).

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use code_index_core::storage::Storage;

/// Декларативные привязки процедуры: записи для ответа `get_callers`.
///
/// Процедура адресуется ИМЕНЕМ, а не путём. Отсюда две ветки:
///
/// - имя носит ОДНА процедура — отдаём её привязки целиком, это точный ответ;
/// - имя носят несколько (у обработчиков форм имя обычно совпадает с именем
///   события: `ПриОткрытии` — это тысячи разных процедур) — поимённый список
///   был бы бесполезен и огромен, поэтому отдаём одну запись: сколько таких
///   процедур и как спросить про нужную форму.
pub fn form_bindings(storage: &Storage, function_name: &str) -> Vec<Value> {
    if function_name.is_empty() {
        return Vec::new();
    }
    let conn = storage.conn();
    let mut confirmed = confirmed_bindings(conn, function_name);
    match confirmed.len() {
        0 => Vec::new(),
        1 => confirmed.remove(0).1,
        // Отличить «одна процедура» от «несколько» — всё, что нужно; поэтому
        // обход и остановлен на второй. Точного числа привязок мы не считали
        // и не называем: у горячего имени это тысячи разборов описаний форм.
        _ => vec![ambiguous_name(&confirmed[0].0, function_name)],
    }
}

/// Формы, где процедура с таким именем ДЕЙСТВИТЕЛЬНО назначена обработчиком.
///
/// Одноимённые процедуры в модулях форм — ещё не привязка: имя может просто
/// повторяться (локальная функция с именем процедуры общего модуля). Поэтому
/// каждый кандидат сверяется с описанием его формы. Обход прекращается на
/// второй подтверждённой: дальше ответ всё равно один и тот же.
fn confirmed_bindings(conn: &Connection, function_name: &str) -> Vec<(FormLocation, Vec<Value>)> {
    let sql = "SELECT f.path FROM functions fn \
               JOIN files f ON f.id = fn.file_id \
               WHERE fn.name = ?1 \
                 AND (f.path LIKE '%/Forms/%' OR f.path LIKE '%/CommonForms/%') \
               ORDER BY f.path";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(params![function_name], |r| r.get::<_, String>(0)) else {
        return Vec::new();
    };
    let mut out: Vec<(FormLocation, Vec<Value>)> = Vec::new();
    for path in rows.flatten() {
        let Some(form) = FormLocation::from_module_path(&path) else {
            continue;
        };
        let bindings = bindings_of_form(conn, &form, function_name);
        if bindings.is_empty() {
            continue;
        }
        out.push((form, bindings));
        if out.len() > 1 {
            break;
        }
    }
    out
}

/// Имя принадлежит не одной процедуре: вместо списка — куда идти дальше.
/// Форма в примере — первая найденная, она приведена как образец вызова,
/// а не как ответ.
fn ambiguous_name(example: &FormLocation, function_name: &str) -> Value {
    json!({
        "kind": "form_binding_ambiguous",
        "callee": function_name,
        "caller": format!(
            "Обработчик формы: имя «{}» носит не одна процедура — они в разных формах, \
             у каждой своя привязка",
            function_name
        ),
        "hint": format!(
            "Поимённый список не отдаём: он ничего не говорит о нужной вам процедуре. \
             Спросите обработчики КОНКРЕТНОЙ формы — образец вызова, форма в нём взята \
             для примера: get_form_handlers(owner_full_name='{}', form_name='{}')",
            example.owner_full_name, example.form_name
        ),
    })
}

/// Форма, которой принадлежит модуль: владелец в формате папки выгрузки
/// (`Catalogs.Контрагенты` — именно так лежит в `metadata_forms`), имя формы
/// и путь к файлу описания.
struct FormLocation {
    owner_full_name: String,
    form_name: String,
    descriptor_path: String,
}

impl FormLocation {
    /// Разбор пути модуля формы. Поддержаны обе раскладки выгрузки:
    ///
    /// - конфигуратор: `<...>/Catalogs/X/Forms/Y/Ext/Form/Module.bsl`
    ///   → описание `<...>/Catalogs/X/Forms/Y/Ext/Form.xml`
    /// - EDT: `<...>/Catalogs/X/Forms/Y/Module.bsl`
    ///   → описание `<...>/Catalogs/X/Forms/Y/Form.form`
    ///
    /// Всё остальное (модуль объекта, общий модуль, модуль команды) — `None`.
    fn from_module_path(path: &str) -> Option<Self> {
        let parts: Vec<&str> = path.split('/').collect();

        // Общая форма: `<...>/CommonForms/<Форма>/Ext/Form/Module.bsl`. Владельца
        // вида-объекта у неё нет — владелец и есть папка вида, а имя формы
        // совпадает с именем объекта: ровно так эта форма записана в
        // `metadata_forms` (`CommonForms.<Имя>` / `<Имя>`). Без этой ветки
        // обработчики общих форм не находились вовсе.
        if let Some(at) = parts.iter().position(|p| *p == "CommonForms") {
            let name = *parts.get(at + 1)?;
            let tail = parts.get(at + 2..)?;
            let root = parts[..=at + 1].join("/");
            let descriptor_path = match tail {
                ["Ext", "Form", "Module.bsl"] => format!("{}/Ext/Form.xml", root),
                ["Module.bsl"] => format!("{}/Form.form", root),
                _ => return None,
            };
            return Some(Self {
                owner_full_name: format!("CommonForms.{}", name),
                form_name: name.to_string(),
                descriptor_path,
            });
        }

        let forms_at = parts.iter().position(|p| *p == "Forms")?;
        // Нужны папка вида-объекта и сам объект слева, имя формы справа.
        if forms_at < 2 || forms_at + 1 >= parts.len() {
            return None;
        }
        let folder = parts[forms_at - 2];
        let object = parts[forms_at - 1];
        let form_name = parts[forms_at + 1];
        let tail = &parts[forms_at + 2..];
        let form_root = parts[..=forms_at + 1].join("/");
        let descriptor_path = match tail {
            ["Ext", "Form", "Module.bsl"] => format!("{}/Ext/Form.xml", form_root),
            ["Module.bsl"] => format!("{}/Form.form", form_root),
            _ => return None,
        };
        Some(Self {
            owner_full_name: format!("{}.{}", folder, object),
            form_name: form_name.to_string(),
            descriptor_path,
        })
    }
}

/// Обработчики формы, назначенные на процедуру с этим именем.
fn bindings_of_form(conn: &Connection, form: &FormLocation, function_name: &str) -> Vec<Value> {
    let handlers_json: Option<String> = conn
        .query_row(
            "SELECT handlers_json FROM metadata_forms \
             WHERE repo = ? AND owner_full_name = ? AND form_name = ?",
            params!["default", &form.owner_full_name, &form.form_name],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten();
    let Some(handlers) = handlers_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
    else {
        return Vec::new();
    };
    let Some(handlers) = handlers.as_array() else {
        return Vec::new();
    };

    // Имена в 1С регистронезависимы; ASCII-сравнение кириллицу не покрывает.
    let wanted = function_name.to_lowercase();
    // Путь описания даём только если файл реально в индексе: он уходит в
    // dependent_files, и выдуманный путь сломал бы сброс кэша по файлу.
    let descriptor = descriptor_if_indexed(conn, &form.descriptor_path);

    handlers
        .iter()
        .filter(|h| {
            h.get("handler")
                .and_then(|v| v.as_str())
                .is_some_and(|name| name.to_lowercase() == wanted)
        })
        .map(|h| {
            let event = h.get("event").and_then(|v| v.as_str()).unwrap_or("");
            let element = h.get("element").and_then(|v| v.as_str());
            // Вид привязки: событие формы или команда формы. Платформа зовёт
            // процедуру в обоих случаях, но имя у команды своё — и в ответе это
            // должно читаться, иначе «событие ЗаполнитьБилеты» сбивает с толку.
            let binding = match h.get("kind").and_then(|v| v.as_str()) {
                Some("command") => "command",
                _ => "event",
            };
            let mut rec = json!({
                "kind": "form_binding",
                "binding": binding,
                "caller": caller_label(form, event, element, binding),
                "callee": h.get("handler").cloned().unwrap_or(Value::Null),
                "event": event,
            });
            if let Some(el) = element {
                rec["element"] = json!(el);
            }
            if let Some(path) = descriptor.as_deref() {
                rec["path"] = json!(path);
            }
            rec
        })
        .collect()
}

/// Человекочитаемое «кто вызывает»: форма, вид привязки и — если обработчик
/// назначен элементу, а не самой форме — имя элемента.
fn caller_label(form: &FormLocation, event: &str, element: Option<&str>, binding: &str) -> String {
    let what = if binding == "command" {
        "команда"
    } else {
        "событие"
    };
    match element {
        Some(el) => format!(
            "Форма {}/{}, элемент {}, {} {}",
            form.owner_full_name, form.form_name, el, what, event
        ),
        None => format!(
            "Форма {}/{}, {} {}",
            form.owner_full_name, form.form_name, what, event
        ),
    }
}

fn descriptor_if_indexed(conn: &Connection, path: &str) -> Option<String> {
    conn.query_row("SELECT 1 FROM files WHERE path = ?1", params![path], |_| {
        Ok(())
    })
    .ok()
    .map(|_| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configurator_module_path_resolves_to_form_xml() {
        let loc = FormLocation::from_module_path(
            "base/Catalogs/Контрагенты/Forms/ФормаЭлемента/Ext/Form/Module.bsl",
        )
        .expect("модуль формы конфигуратора должен разбираться");
        assert_eq!(loc.owner_full_name, "Catalogs.Контрагенты");
        assert_eq!(loc.form_name, "ФормаЭлемента");
        assert_eq!(
            loc.descriptor_path,
            "base/Catalogs/Контрагенты/Forms/ФормаЭлемента/Ext/Form.xml"
        );
    }

    #[test]
    fn edt_module_path_resolves_to_form_file() {
        let loc =
            FormLocation::from_module_path("src/Documents/Заказ/Forms/ФормаДокумента/Module.bsl")
                .expect("модуль формы EDT должен разбираться");
        assert_eq!(loc.owner_full_name, "Documents.Заказ");
        assert_eq!(
            loc.descriptor_path,
            "src/Documents/Заказ/Forms/ФормаДокумента/Form.form"
        );
    }

    #[test]
    fn non_form_modules_are_rejected() {
        // Модуль объекта, общий модуль и модуль команды формой не являются.
        assert!(
            FormLocation::from_module_path("base/Catalogs/Контрагенты/Ext/ObjectModule.bsl")
                .is_none()
        );
        assert!(FormLocation::from_module_path(
            "base/CommonModules/ОбщегоНазначения/Ext/Module.bsl"
        )
        .is_none());
        assert!(FormLocation::from_module_path(
            "base/Catalogs/Контрагенты/Forms/ФормаЭлемента/Ext/Form/Command/Module.bsl"
        )
        .is_none());
    }

    /// Общая форма: владельца вида-объекта у неё нет, владелец — сама папка
    /// вида, а имя формы совпадает с именем объекта.
    #[test]
    fn common_form_module_path_resolves() {
        let loc =
            FormLocation::from_module_path("base/CommonForms/ОбщаяФормаВыбора/Ext/Form/Module.bsl")
                .expect("модуль общей формы должен разбираться");
        assert_eq!(loc.owner_full_name, "CommonForms.ОбщаяФормаВыбора");
        assert_eq!(loc.form_name, "ОбщаяФормаВыбора");
        assert_eq!(
            loc.descriptor_path,
            "base/CommonForms/ОбщаяФормаВыбора/Ext/Form.xml"
        );
        // Общий модуль общей формой не становится.
        assert!(
            FormLocation::from_module_path("base/CommonModules/Общее/Ext/Module.bsl").is_none()
        );
    }

    /// Команда формы в ответе называется командой, а не событием.
    #[test]
    fn command_binding_label_says_command() {
        let form = FormLocation {
            owner_full_name: "Documents.X".to_string(),
            form_name: "ФормаДокумента".to_string(),
            descriptor_path: "base/Documents/X/Forms/ФормаДокумента/Ext/Form.xml".to_string(),
        };
        assert_eq!(
            caller_label(&form, "Заполнить", None, "command"),
            "Форма Documents.X/ФормаДокумента, команда Заполнить"
        );
        assert_eq!(
            caller_label(&form, "ПриОткрытии", None, "event"),
            "Форма Documents.X/ФормаДокумента, событие ПриОткрытии"
        );
        assert_eq!(
            caller_label(&form, "ПриИзменении", Some("Сумма"), "event"),
            "Форма Documents.X/ФормаДокумента, элемент Сумма, событие ПриИзменении"
        );
    }

    #[test]
    fn forms_at_root_without_owner_are_rejected() {
        // Без папки вида-объекта и объекта слева владельца не собрать.
        assert!(FormLocation::from_module_path("Forms/Ф/Module.bsl").is_none());
    }
}

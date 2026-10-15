// Парсеры XML-выгрузок 1С, специфичные для bsl-extension.
//
// Эти парсеры дополняют generic `Xml1CParser` из core (который видит XML
// как набор «классов»). Здесь — структурированное извлечение метаданных,
// предназначенное для записи в специфичные таблицы:
//
// - `configuration` — Configuration.xml: список всех объектов конфигурации
//   (Catalog/Document/InformationRegister/...) с их именами, синонимами
//   и UUID. Источник для таблицы `metadata_objects`.
// - `forms` — *.xml в Forms/: имена обработчиков событий формы. Источник
//   для `metadata_forms`.
// - `event_subscriptions` — *.xml в EventSubscriptions/: связь
//   «событие → модуль.процедура». Источник для `event_subscriptions`.
// - `object_attributes` — XML отдельных объектов (Catalogs/<X>.xml и т.д.):
//   ссылочные типы реквизитов/измерений и свойства-ссылки шапки
//   (`<MainDataCompositionSchema>`) → рёбра графа связей данных.
//   Источник для `data_links`.
// - `metadata_refs` — связи КОНФИГУРАЦИОННОГО уровня (состав подсистем и
//   планов обмена, типы определяемых типов, расположение функциональных
//   опций) → доп. рёбра `data_links`; плюс права ролей (Rights.xml) для
//   отдельной таблицы `role_rights`.
// - `dcs` — макеты «Схема компоновки данных» (`Templates/<Имя>/Ext/Template.xml`
//   и `Template.dcs` выгрузки EDT): наборы данных с полями, связи наборов,
//   вычисляемые поля, итоги, параметры и варианты настроек. Источник для
//   таблиц `dcs_schemas` / `dcs_datasets` и рёбер `data_links` вида `dcs_query`.

use std::borrow::Cow;

use quick_xml::events::BytesText;

pub(crate) use code_index_core::parser::xml_1c::general_ref_text;

/// Совместимый с прежним quick-xml путь: декодировать XML-текст и раскрыть
/// стандартные entity (`&amp;`, `&lt;` и т.д.).
pub(crate) trait BytesTextExt {
    fn unescape(&self) -> Result<Cow<'_, str>, ()>;
}

impl BytesTextExt for BytesText<'_> {
    fn unescape(&self) -> Result<Cow<'_, str>, ()> {
        let decoded = self.xml10_content().map_err(|_| ())?;
        let unescaped = quick_xml::escape::unescape(&decoded).map_err(|_| ())?;
        Ok(Cow::Owned(unescaped.into_owned()))
    }
}

/// Имя свойства метаданных, значение которого — ссылка на другой объект:
/// основная схема компоновки данных отчёта (`<MainDataCompositionSchema>` в
/// формате Конфигуратора, `<mainDataCompositionSchema>` в EDT). Идёт в
/// `data_links` ребром `link_kind = "metadata_property"`: `from_object` —
/// владелец свойства, `from_path` — это имя, `to_object` — цель ссылки.
pub(crate) const PROPERTY_REF_MAIN_DCS: &str = "MainDataCompositionSchema";

/// Значение свойства метаданных, чья форма — готовая ссылка на объект
/// (`<MainDataCompositionSchema>Report.X.Template.ОсновнаяСхемаКомпоновкиДанных</MainDataCompositionSchema>`,
/// `<VariantsStorage>SettingsStorage.Y</VariantsStorage>`). Возвращает
/// каноническое имя, если строка на ссылку похожа: без пробелов, первый сегмент
/// — известный тип метаданных ([`configuration::KNOWN_META_TYPES`]), сегменты
/// непустые.
///
/// Ссылкой НЕ считаются: пустое значение (`<MainDataCompositionSchema/>` —
/// основная схема не задана), идентификатор выгрузки расширения вместо имени,
/// скаляр (`true`, `SpreadsheetDocument`) и любое значение со `*` (обобщённый
/// тип — из XML свойств не приходит).
pub(crate) fn metadata_reference(value: &str) -> Option<&str> {
    let v = value.trim();
    if v.len() < 3 || v.chars().any(char::is_whitespace) || v.starts_with('*') {
        return None;
    }
    let (head, rest) = v.split_once('.')?;
    if !configuration::KNOWN_META_TYPES.contains(&head) {
        return None;
    }
    if rest.split('.').any(|seg| seg.is_empty()) {
        return None;
    }
    Some(v)
}

pub mod config_dump_info;
pub mod configuration;
pub mod dcs;
// `edt_mdo` — формат 1C:EDT (`.mdo`): структура объектов, связи данных,
// синоним/шапка. Заполняет те же таблицы, что и формат Конфигуратора.
pub mod edt_mdo;
pub mod event_subscriptions;
pub mod forms;
pub mod metadata_refs;
pub mod object_attributes;
pub mod object_uuid;

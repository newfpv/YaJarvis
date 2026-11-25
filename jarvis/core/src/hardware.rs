//! Какая это колонка: по дереву устройств и настройкам Алисы.

use serde::Serialize;
use std::sync::OnceLock;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Model {
    pub id: &'static str,
    pub name: &'static str,
}

/// Compatible и DT ID важнее названий. station-midi/yandexmidi — это Станция 2; обычная Миди — cucumber.
const RULES: &[(&str, &str, &[&str])] = &[
    ("mini3pro", "Яндекс Станция Мини 3 Про", &["pickle", "mini 3 pro", "мини 3 про"]),
    ("mini3", "Яндекс Станция Мини 3", &["bergamot", "mini 3", "мини 3"]),
    ("midi", "Яндекс Станция Миди", &["cucumber"]),
    ("station3", "Яндекс Станция 3", &["orion"]),
    ("lite2", "Яндекс Станция Лайт 2", &["plum", "yandex,prune", "lite 2", "лайт 2", "lite2", "micro2", "micro_2", "micro-2"]),
    ("street", "Яндекс Станция Стрит", &["mango", "dmpv", "street"]),
    ("fiero", "Fiero Hi", &["blackthorn", "fiero"]),
    ("mini2", "Яндекс Станция Мини 2", &["station-mini2", "station-mini-2", "yandexmini_2", "mini 2", "мини 2"]),
    ("station2", "Яндекс Станция 2", &["station-midi", "yandexmidi", "station midi"]),
    ("max", "Яндекс Станция Макс", &["yandexstation_2", "station max", "станция макс"]),
    ("station1", "Яндекс Станция первого поколения", &["tulip", "yandexstation"]),
    ("hub", "Умный хаб Яндекса", &["saturn"]),
    ("lite", "Яндекс Станция Лайт", &["station-micro", "station micro", "yandexmicro", "station lite", "станция лайт"]),
    ("mini1", "Яндекс Станция Мини", &["station-mini", "yandexmini", "station mini"]),
    ("jbl", "JBL Link", &["jbl", "mediatek,mt8167"]),
];

pub fn detect(compatible: &str, dtid: &str, product: &str, display: &str) -> Model {
    for raw in [compatible, dtid, product, display] {
        let raw = raw.to_lowercase();
        for (id, name, markers) in RULES {
            if markers.iter().any(|m| raw.contains(m)) {
                return Model { id, name };
            }
        }
    }
    Model { id: "unknown", name: "Яндекс Станция (модель не определена)" }
}

pub fn station_model() -> &'static Model {
    static M: OnceLock<Model> = OnceLock::new();
    M.get_or_init(|| {
        let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
        // тип устройства Алисы (quasar.cfg: "deviceType") — запасной признак
        let cfg = read("/system/vendor/quasar/quasar.cfg");
        let product = cfg
            .find("\"deviceType\"")
            .and_then(|i| {
                let rest = &cfg[i + 12..];
                let rest = &rest[rest.find(':')? + 1..];
                let rest = &rest[rest.find('"')? + 1..];
                Some(rest[..rest.find('"')?].to_string())
            })
            .unwrap_or_default();
        detect(
            &read("/proc/device-tree/compatible"),
            &read("/proc/device-tree/amlogic-dt-id"),
            &product,
            &read("/proc/device-tree/model"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_models() {
        assert_eq!(detect("yandex,station-micro", "", "", "").id, "lite");
        assert_eq!(detect("", "", "yandexmidi", "").id, "station2");
        assert_eq!(detect("amlogic,cucumber", "", "", "").id, "midi");
        assert_eq!(detect("yandex,mango", "", "", "").id, "street");
        assert_eq!(detect("", "", "", "Yandex-Station-Street-DMPV").id, "street");
        assert_eq!(detect("", "", "", "").id, "unknown");
    }
}

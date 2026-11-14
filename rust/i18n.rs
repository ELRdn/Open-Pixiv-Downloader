use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "en")]
    English,
    #[serde(rename = "ja")]
    Japanese,
}

impl Language {
    pub fn text<'a>(self, english: &'a str, japanese: &'a str) -> &'a str {
        match self {
            Self::English => english,
            Self::Japanese => japanese,
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::English => Self::Japanese,
            Self::Japanese => Self::English,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Message {
    pub english: String,
    pub japanese: String,
}

impl Message {
    pub fn render(self, language: Language) -> String {
        match language {
            Language::English => self.english,
            Language::Japanese => self.japanese,
        }
    }
}

#[macro_export]
macro_rules! tr_format {
    ($language:expr, $english:literal, $japanese:literal $(, $argument:expr)* $(,)?) => {
        match $language {
            $crate::i18n::Language::English => format!($english $(, $argument)*),
            $crate::i18n::Language::Japanese => format!($japanese $(, $argument)*),
        }
    };
}

#[macro_export]
macro_rules! message {
    ($english:literal, $japanese:literal $(, $argument:expr)* $(,)?) => {
        $crate::i18n::Message {
            english: format!($english $(, $argument)*),
            japanese: format!($japanese $(, $argument)*),
        }
    };
}

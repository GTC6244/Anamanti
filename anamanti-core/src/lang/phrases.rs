//! Localized spoken replies for every deterministic (non-LLM) path.
//!
//! The rule (`plans/BilingualPlan.md` §2.2): no spoken English literal lives outside
//! this module (and the Home Assistant replies in `home_assistant::pt`). Each function
//! takes the turn's [`Lang`]; the [`Lang::En`] arm is **byte-for-byte the pre-bilingual
//! string**, so English output never changes (the tests here pin that).
//!
//! Portuguese text is written for Piper's `pt_BR` voices (espeak-ng phonemizer): the
//! feminine numbers that espeak would read as masculine (`1 hora` → "um hora") are
//! spelled out ("uma hora", "duas horas"), and units are words, not symbols.

use chrono::{DateTime, Datelike, TimeZone, Timelike};

use super::Lang;
use crate::speaker::SpeakerResolution;

// ---------------------------------------------------------------- generic acks

/// A bare acknowledgement (dismissing a screen, ending the conversation).
pub fn okay(lang: Lang) -> &'static str {
    match lang {
        Lang::En => "Okay.",
        Lang::PtBr => "Tudo bem.",
    }
}

// ---------------------------------------------------------------- durations

/// The timer phrasing of a duration (the [`crate::speech::DurationStyle::Exact`] style):
/// "10 minutes" / "10 minutos", "1 hour 30 minutes" / "uma hora e 30 minutos".
pub fn duration(lang: Lang, secs: u64) -> String {
    match lang {
        Lang::En => crate::speech::duration(secs as f64, crate::speech::DurationStyle::Exact),
        Lang::PtBr => pt_duration(secs).0,
    }
}

/// Portuguese "h horas, m minutos e s segundos", plus whether the phrase is grammatically
/// singular (exactly one of one unit — "um minuto"), for verb agreement ("Falta" vs
/// "Faltam").
fn pt_duration(secs: u64) -> (String, bool) {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let mut parts: Vec<String> = Vec::new();
    if h > 0 {
        parts.push(match h {
            1 => "uma hora".to_string(),
            2 => "duas horas".to_string(),
            n => format!("{n} horas"),
        });
    }
    if m > 0 {
        parts.push(match m {
            1 => "um minuto".to_string(),
            n => format!("{n} minutos"),
        });
    }
    if s > 0 {
        parts.push(match s {
            1 => "um segundo".to_string(),
            n => format!("{n} segundos"),
        });
    }
    let singular = parts.len() == 1 && (h == 1 || m == 1 || s == 1) && (h + m + s == 1);
    let text = match parts.len() {
        0 => "0 segundos".to_string(),
        1 => parts.remove(0),
        _ => {
            let last = parts.pop().unwrap();
            format!("{} e {last}", parts.join(", "))
        }
    };
    (text, singular)
}

// ---------------------------------------------------------------- timers (System-1)

/// "Timer set for 10 minutes."
pub fn timer_set(lang: Lang, secs: u64) -> String {
    match lang {
        Lang::En => format!("Timer set for {}.", duration(lang, secs)),
        Lang::PtBr => format!("Timer de {} iniciado.", duration(lang, secs)),
    }
}

/// "Okay, I've cancelled your timer(s)."
pub fn timers_cancelled(lang: Lang, several: bool) -> &'static str {
    match (lang, several) {
        (Lang::En, true) => "Okay, I've cancelled your timers.",
        (Lang::En, false) => "Okay, I've cancelled your timer.",
        (Lang::PtBr, true) => "Tudo bem, cancelei os timers.",
        (Lang::PtBr, false) => "Tudo bem, cancelei o timer.",
    }
}

/// "You have 5 minutes left." — or "just about up" when nothing meaningful remains.
pub fn timer_remaining(lang: Lang, secs: Option<u64>) -> String {
    match (lang, secs) {
        (Lang::En, Some(s)) if s > 0 => format!("You have {} left.", duration(lang, s)),
        (Lang::En, _) => "Your timer is just about up.".to_string(),
        (Lang::PtBr, Some(s)) if s > 0 => {
            let (d, singular) = pt_duration(s);
            if singular {
                format!("Falta {d}.")
            } else {
                format!("Faltam {d}.")
            }
        }
        (Lang::PtBr, _) => "O timer está quase acabando.".to_string(),
    }
}

/// "Added 5 minutes to your timer." / "Took 1 minute off your timer."
pub fn timer_adjusted(lang: Lang, added: bool, secs: u64) -> String {
    let d = duration(lang, secs);
    match (lang, added) {
        (Lang::En, true) => format!("Added {d} to your timer."),
        (Lang::En, false) => format!("Took {d} off your timer."),
        (Lang::PtBr, true) => format!("Adicionei {d} ao timer."),
        (Lang::PtBr, false) => format!("Tirei {d} do timer."),
    }
}

// ---------------------------------------------------------------- clock

/// "It's 3:45 PM." / "São 15 horas e 45 minutos."
pub fn time_now<Tz: TimeZone>(lang: Lang, now: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    match lang {
        Lang::En => format!("It's {}.", now.format("%-I:%M %p")),
        Lang::PtBr => {
            let (h, m) = (now.hour(), now.minute());
            let hours = match h {
                0 => "meia-noite".to_string(),
                1 => "uma hora".to_string(),
                2 => "duas horas".to_string(),
                12 => "meio-dia".to_string(),
                21 => "vinte e uma horas".to_string(),
                22 => "vinte e duas horas".to_string(),
                n => format!("{n} horas"),
            };
            // "É meia-noite / meio-dia / uma hora", otherwise plural "São …".
            let verb = if matches!(h, 0 | 1 | 12) {
                "É"
            } else {
                "São"
            };
            match m {
                0 => format!("{verb} {hours}."),
                1 => format!("{verb} {hours} e um minuto."),
                n => format!("{verb} {hours} e {n} minutos."),
            }
        }
    }
}

const PT_WEEKDAYS: [&str; 7] = [
    "segunda-feira",
    "terça-feira",
    "quarta-feira",
    "quinta-feira",
    "sexta-feira",
    "sábado",
    "domingo",
];

const PT_MONTHS: [&str; 12] = [
    "janeiro",
    "fevereiro",
    "março",
    "abril",
    "maio",
    "junho",
    "julho",
    "agosto",
    "setembro",
    "outubro",
    "novembro",
    "dezembro",
];

/// "Today is Monday, October 19." / "Hoje é segunda-feira, 19 de outubro."
pub fn date_today<Tz: TimeZone>(lang: Lang, now: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    match lang {
        Lang::En => format!("Today is {}.", now.format("%A, %B %-d")),
        Lang::PtBr => {
            let wd = PT_WEEKDAYS[now.weekday().num_days_from_monday() as usize];
            let month = PT_MONTHS[now.month0() as usize];
            let day = match now.day() {
                1 => "primeiro".to_string(),
                d => d.to_string(),
            };
            format!("Hoje é {wd}, {day} de {month}.")
        }
    }
}

// ---------------------------------------------------------------- weather

/// A WMO weather-code description in `lang`. The English table is the one the display
/// frame carries (`crate::weather::weather_description`); Portuguese is used only for
/// speech — the weather screen stays English (owner decision).
pub fn weather_description(lang: Lang, code: i32) -> &'static str {
    match lang {
        Lang::En => crate::weather::weather_description(code),
        Lang::PtBr => match code {
            0 => "céu limpo",
            1 => "predominantemente limpo",
            2 => "parcialmente nublado",
            3 => "nublado",
            45 | 48 => "neblina",
            51 | 53 | 55 => "garoa",
            56 | 57 => "garoa congelante",
            61 => "chuva fraca",
            63 => "chuva",
            65 => "chuva forte",
            66 | 67 => "chuva congelante",
            71 => "neve fraca",
            73 => "neve",
            75 => "neve forte",
            77 => "grãos de neve",
            80 | 81 => "pancadas de chuva",
            82 => "pancadas de chuva forte",
            85 | 86 => "pancadas de neve",
            95 => "tempestade",
            96 | 99 => "tempestade com granizo",
            _ => "condições desconhecidas",
        },
    }
}

/// The System-1 weather fast path's one-line summary.
pub fn weather_summary(lang: Lang, report: &crate::weather::WeatherReport) -> String {
    let c = &report.current;
    match lang {
        Lang::En => {
            let deg = if report.units == "imperial" {
                "°F"
            } else {
                "°C"
            };
            let lead = {
                let d = c.description.trim();
                if d.is_empty() {
                    String::new()
                } else {
                    format!("{d}, ")
                }
            };
            format!(
                "{lead}it's {}{deg} in {}. Today's high is {}{deg}, low {}{deg}.",
                c.temp, report.location_label, c.high, c.low
            )
        }
        Lang::PtBr => {
            let unit = if report.units == "imperial" {
                " Fahrenheit"
            } else {
                ""
            };
            let desc = weather_description(lang, c.weather_code);
            let desc = capitalize(desc);
            format!(
                "{desc}, fazendo {} graus{unit} em {}. A máxima hoje é de {} graus e a mínima \
                 de {}.",
                c.temp, report.location_label, c.high, c.low
            )
        }
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------- memory commands

/// Confirmation for "remember …".
pub fn remembered(lang: Lang) -> &'static str {
    match lang {
        Lang::En => "Okay, I'll remember that.",
        Lang::PtBr => "Tudo bem, vou lembrar disso.",
    }
}

/// Confirmation for "forget …" that deleted something.
pub fn forgotten(lang: Lang) -> &'static str {
    match lang {
        Lang::En => "Okay, I've forgotten that.",
        Lang::PtBr => "Tudo bem, esqueci isso.",
    }
}

/// "forget <topic>" matched nothing.
pub fn nothing_matched(lang: Lang) -> &'static str {
    match lang {
        Lang::En => "I didn't have anything about that.",
        Lang::PtBr => "Eu não tinha nada sobre isso.",
    }
}

/// "forget that" with nothing stored.
pub fn nothing_to_forget(lang: Lang) -> &'static str {
    match lang {
        Lang::En => "There was nothing to forget.",
        Lang::PtBr => "Não havia nada para esquecer.",
    }
}

/// The stored fact for "my name is X" — stored in the language it was said.
pub fn name_fact(lang: Lang, name: &str) -> String {
    match lang {
        Lang::En => format!("The user's name is {name}"),
        Lang::PtBr => format!("O nome do usuário é {name}"),
    }
}

/// The pointer to guided enrollment.
fn enroll_hint(lang: Lang) -> &'static str {
    match lang {
        Lang::En => {
            "On the display, go to Settings, People, Add a voice, and pick your name so I'll \
             recognise you."
        }
        Lang::PtBr => {
            "Na tela, vá em Settings, People, Add a voice, e escolha o seu nome para eu \
             reconhecer você."
        }
    }
}

/// The replies of the "my name is X" voice-naming flow.
#[derive(Debug, Clone, Copy)]
pub enum NameReply<'a> {
    /// The voice wasn't confidently identified.
    UnknownVoice { display: &'a str },
    /// The voice already has this name.
    AlreadyKnown { known: &'a str },
    /// The voice already has a different name.
    KnownAsOther { known: &'a str },
    /// The spoken name isn't in the household roster.
    NotInHousehold { display: &'a str },
    /// Another voiceprint already has the name.
    NameTaken { name: &'a str },
    /// Named.
    Named { name: &'a str },
}

/// Speak a [`NameReply`].
pub fn name_reply(lang: Lang, r: NameReply<'_>) -> String {
    let hint = enroll_hint(lang);
    match (lang, r) {
        (Lang::En, NameReply::UnknownVoice { display }) => {
            format!("Nice to meet you, {display}. I don't know your voice yet. {hint}")
        }
        (Lang::En, NameReply::AlreadyKnown { known }) => format!("Yes, I know. Hi, {known}!"),
        (Lang::En, NameReply::KnownAsOther { known }) => format!(
            "I know your voice as {known}. If that's wrong, you can rename it under Settings, \
             People on the display."
        ),
        (Lang::En, NameReply::NotInHousehold { display }) => format!(
            "Nice to meet you, {display}. I couldn't find you in the household list. {hint}"
        ),
        (Lang::En, NameReply::NameTaken { name }) => format!(
            "I already know a {name} with a different voice, so I won't change anything yet. \
             {name}, you can add more voice samples under Settings, People on the display."
        ),
        (Lang::En, NameReply::Named { name }) => format!("Nice to meet you, {name}!"),
        (Lang::PtBr, NameReply::UnknownVoice { display }) => {
            format!("Prazer em conhecer você, {display}. Ainda não conheço a sua voz. {hint}")
        }
        (Lang::PtBr, NameReply::AlreadyKnown { known }) => format!("Sim, eu sei. Oi, {known}!"),
        (Lang::PtBr, NameReply::KnownAsOther { known }) => format!(
            "Eu conheço a sua voz como {known}. Se estiver errado, você pode mudar o nome em \
             Settings, People na tela."
        ),
        (Lang::PtBr, NameReply::NotInHousehold { display }) => format!(
            "Prazer em conhecer você, {display}. Não encontrei você na lista da casa. {hint}"
        ),
        (Lang::PtBr, NameReply::NameTaken { name }) => format!(
            "Eu já conheço alguém chamado {name} com outra voz, então não vou mudar nada por \
             enquanto. {name}, você pode adicionar mais amostras de voz em Settings, People na \
             tela."
        ),
        (Lang::PtBr, NameReply::Named { name }) => format!("Prazer em conhecer você, {name}!"),
    }
}

/// The answer to "who am I?", from the voiceprint resolution (never memory text).
pub fn who_am_i(lang: Lang, resolution: SpeakerResolution, name: Option<&str>) -> String {
    use SpeakerResolution::*;
    match lang {
        Lang::En => {
            let enroll = "To teach me, go to Settings, People, Add a voice on the display.";
            match (resolution, name) {
                (Identified, Some(n)) => format!("You're {n}."),
                (Tentative, Some(n)) => format!("I think you're {n}, but I'm not completely sure."),
                (Identified, None) => {
                    format!("I recognise your voice, but I don't know your name yet. {enroll}")
                }
                (Tentative, None) => format!("I'm not sure who you are yet. {enroll}"),
                (New | Unknown, _) => format!("I don't recognise your voice yet. {enroll}"),
            }
        }
        Lang::PtBr => {
            let enroll = "Para me ensinar, vá em Settings, People, Add a voice na tela.";
            match (resolution, name) {
                (Identified, Some(n)) => format!("Você é {n}."),
                (Tentative, Some(n)) => {
                    format!("Acho que você é {n}, mas não tenho certeza absoluta.")
                }
                (Identified, None) => {
                    format!("Eu reconheço a sua voz, mas ainda não sei o seu nome. {enroll}")
                }
                (Tentative, None) => format!("Ainda não tenho certeza de quem você é. {enroll}"),
                (New | Unknown, _) => format!("Ainda não reconheço a sua voz. {enroll}"),
            }
        }
    }
}

// ---------------------------------------------------------------- personality

/// The personality-switch confirmations. `label` = the new personality's display label;
/// `None` = back to normal.
pub fn personality_set(lang: Lang, label: Option<&str>) -> String {
    match (lang, label) {
        (Lang::En, Some(l)) => format!("Okay, I'll talk in my {l} voice from now on."),
        (Lang::En, None) => "Okay, back to my normal voice.".to_string(),
        (Lang::PtBr, Some(l)) => format!("Tudo bem, a partir de agora vou falar no modo {l}."),
        (Lang::PtBr, None) => "Tudo bem, voltei para a minha voz normal.".to_string(),
    }
}

/// The personality name didn't match the catalog.
pub fn personality_unknown(lang: Lang) -> &'static str {
    match lang {
        Lang::En => "Sorry, I don't have a personality by that name.",
        Lang::PtBr => "Desculpe, não tenho uma personalidade com esse nome.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<FixedOffset> {
        FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .unwrap()
    }

    #[test]
    fn english_strings_are_unchanged() {
        assert_eq!(okay(Lang::En), "Okay.");
        assert_eq!(timer_set(Lang::En, 600), "Timer set for 10 minutes.");
        assert_eq!(
            timers_cancelled(Lang::En, true),
            "Okay, I've cancelled your timers."
        );
        assert_eq!(
            timer_remaining(Lang::En, Some(90)),
            "You have 1 minute 30 seconds left."
        );
        assert_eq!(
            timer_remaining(Lang::En, Some(0)),
            "Your timer is just about up."
        );
        assert_eq!(
            timer_adjusted(Lang::En, true, 300),
            "Added 5 minutes to your timer."
        );
        assert_eq!(
            timer_adjusted(Lang::En, false, 60),
            "Took 1 minute off your timer."
        );
        assert_eq!(
            time_now(Lang::En, &at(2026, 10, 19, 15, 45)),
            "It's 3:45 PM."
        );
        assert_eq!(
            date_today(Lang::En, &at(2026, 10, 19, 15, 45)),
            "Today is Monday, October 19."
        );
        assert_eq!(remembered(Lang::En), "Okay, I'll remember that.");
        assert_eq!(name_fact(Lang::En, "Sam"), "The user's name is Sam");
        assert_eq!(
            who_am_i(Lang::En, SpeakerResolution::Identified, Some("Sam")),
            "You're Sam."
        );
        assert_eq!(
            personality_set(Lang::En, Some("gangster")),
            "Okay, I'll talk in my gangster voice from now on."
        );
        // The English duration is exactly the shared timer humanizer.
        for s in [0, 1, 59, 60, 61, 90, 3600, 3661, 5400, 86_400] {
            assert_eq!(
                duration(Lang::En, s),
                crate::speech::duration(s as f64, crate::speech::DurationStyle::Exact)
            );
        }
    }

    #[test]
    fn portuguese_durations_agree_in_gender_and_number() {
        assert_eq!(duration(Lang::PtBr, 600), "10 minutos");
        assert_eq!(duration(Lang::PtBr, 60), "um minuto");
        assert_eq!(duration(Lang::PtBr, 3600), "uma hora");
        assert_eq!(duration(Lang::PtBr, 7200), "duas horas");
        assert_eq!(duration(Lang::PtBr, 5400), "uma hora e 30 minutos");
        assert_eq!(
            duration(Lang::PtBr, 3661),
            "uma hora, um minuto e um segundo"
        );
        assert_eq!(duration(Lang::PtBr, 0), "0 segundos");
        assert_eq!(timer_remaining(Lang::PtBr, Some(60)), "Falta um minuto.");
        assert_eq!(timer_remaining(Lang::PtBr, Some(600)), "Faltam 10 minutos.");
        assert_eq!(timer_set(Lang::PtBr, 600), "Timer de 10 minutos iniciado.");
    }

    #[test]
    fn portuguese_clock_reads_naturally() {
        assert_eq!(
            time_now(Lang::PtBr, &at(2026, 10, 19, 15, 45)),
            "São 15 horas e 45 minutos."
        );
        assert_eq!(
            time_now(Lang::PtBr, &at(2026, 10, 19, 13, 0)),
            "São 13 horas."
        );
        assert_eq!(
            time_now(Lang::PtBr, &at(2026, 10, 19, 1, 1)),
            "É uma hora e um minuto."
        );
        assert_eq!(
            time_now(Lang::PtBr, &at(2026, 10, 19, 12, 0)),
            "É meio-dia."
        );
        assert_eq!(
            time_now(Lang::PtBr, &at(2026, 10, 19, 21, 5)),
            "São vinte e uma horas e 5 minutos."
        );
        assert_eq!(
            date_today(Lang::PtBr, &at(2026, 10, 19, 9, 0)),
            "Hoje é segunda-feira, 19 de outubro."
        );
        assert_eq!(
            date_today(Lang::PtBr, &at(2026, 3, 1, 9, 0)),
            "Hoje é domingo, primeiro de março."
        );
    }

    #[test]
    fn weather_descriptions_cover_the_english_table() {
        for code in [
            0, 1, 2, 3, 45, 51, 56, 61, 63, 65, 66, 71, 73, 75, 77, 80, 82, 85, 95, 96,
        ] {
            assert_ne!(
                weather_description(Lang::PtBr, code),
                "condições desconhecidas"
            );
            assert_eq!(
                weather_description(Lang::En, code),
                crate::weather::weather_description(code)
            );
        }
    }
}

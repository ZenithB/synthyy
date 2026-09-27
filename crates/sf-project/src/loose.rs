//! Tolerant scalar deserializers for the FROZEN project schema.
//!
//! `src/state/doc.ts` declares TypeScript types over a Yjs document whose maps
//! are `any` — so the declared type is a hint, not a guarantee, and the same
//! field can hold different JS types depending on how it was created. The real
//! example that motivated this: `addLfo()` writes `on: true` (boolean) while
//! the LFO power button writes `on: 0 | 1` (number), even though `LfoJSON.on`
//! is declared `number`. A strict `f64` field rejected the whole document, and
//! because the desktop engine loads the project in one shot, a single loose
//! field silenced the ENTIRE app.
//!
//! These accept the JS-ish spellings of a value and normalise them. Being
//! permissive on READ costs nothing and keeps the frozen schema's real-world
//! sloppiness from being fatal; serialisation stays canonical.

use serde::{Deserialize, Deserializer};

#[derive(Deserialize)]
#[serde(untagged)]
enum Scalar {
    Bool(bool),
    Num(f64),
    Str(String),
    Null,
}

fn scalar_to_f64(s: Scalar) -> Option<f64> {
    match s {
        Scalar::Bool(b) => Some(if b { 1.0 } else { 0.0 }),
        Scalar::Num(n) => Some(n),
        // numeric strings show up in hand-edited / older files
        Scalar::Str(t) => t.trim().parse::<f64>().ok(),
        Scalar::Null => None,
    }
}

/// `f64` that also accepts `true`/`false`, a numeric string, or null (→ 0.0).
pub fn de_num<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    Ok(scalar_to_f64(Scalar::deserialize(d)?).unwrap_or(0.0))
}

/// `Option<f64>` with the same tolerance; null / unparseable → None.
pub fn de_num_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    Ok(scalar_to_f64(Scalar::deserialize(d)?))
}

/// `bool` that also accepts 0/1 (and other numbers: non-zero is true).
pub fn de_bool<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(match Scalar::deserialize(d)? {
        Scalar::Bool(b) => b,
        Scalar::Num(n) => n != 0.0,
        Scalar::Str(t) => !matches!(t.trim(), "" | "0" | "false"),
        Scalar::Null => false,
    })
}

/// `Option<bool>` with the same tolerance.
pub fn de_bool_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    Ok(match Scalar::deserialize(d)? {
        Scalar::Null => None,
        other => Some(match other {
            Scalar::Bool(b) => b,
            Scalar::Num(n) => n != 0.0,
            Scalar::Str(t) => !matches!(t.trim(), "" | "0" | "false"),
            Scalar::Null => unreachable!(),
        }),
    })
}

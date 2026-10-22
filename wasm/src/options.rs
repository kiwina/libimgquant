//! The options object of the JS functions, parsed and validated in plain Rust so that the
//! rules (and their error messages) are tested natively.

use std::fmt;

use libimgquant::{Attributes, Profile};

/// A JS value, as far as options need it. `undefined` and `null` both mean "not set".
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Undefined,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    /// A plain object's own enumerable properties, in order.
    Object(Vec<(String, Value)>),
    /// Anything else (a function, a symbol, a bigint, an object nested too deep).
    Other,
}

impl fmt::Display for Value {
    /// The value as an error message quotes it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Undefined => f.write_str("undefined"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::Number(n) => write!(f, "{n}"),
            Self::String(s) => write!(f, "'{s}'"),
            Self::Array(_) => f.write_str("an array"),
            Self::Object(_) => f.write_str("an object"),
            Self::Other => f.write_str("a value of another type"),
        }
    }
}

const OPTION_NAMES: &str = "maxColors, speed, quality, dithering and profile";

/// Validated options.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub max_colors: u32,
    pub speed: i32,
    /// `(min, max)`, as for [`Attributes::set_quality`].
    pub quality: Option<(u8, u8)>,
    /// 0-1. The default is 1 (full Floyd-Steinberg), as in pngquant, unlike the Rust API's 0.
    pub dithering: f32,
    pub profile: Profile,
}

impl Default for Settings {
    fn default() -> Self {
        let attr = Attributes::new();
        Self {
            max_colors: attr.max_colors(),
            speed: attr.speed() as i32,
            quality: None,
            dithering: 1.0,
            profile: Profile::default(),
        }
    }
}

impl Settings {
    /// Parses the options argument; `undefined`/`null` give the defaults, unknown names are
    /// errors, so that a misspelt option doesn't go unnoticed.
    pub fn parse(options: &Value) -> Result<Self, String> {
        let mut settings = Self::default();
        let fields = match options {
            Value::Undefined => return Ok(settings),
            Value::Object(fields) => fields,
            other => return Err(format!("options must be an object, not {other}")),
        };
        for (name, value) in fields {
            if *value == Value::Undefined {
                continue;
            }
            match name.as_str() {
                "maxColors" => settings.max_colors = integer(value, 2, 256, "maxColors")?,
                "speed" => settings.speed = integer(value, 1, 10, "speed")? as i32,
                "quality" => settings.quality = Some(quality(value)?),
                "dithering" => settings.dithering = dithering(value)?,
                "profile" => {
                    settings.profile = match value {
                        Value::String(s) if s == "improved" => Profile::Improved,
                        Value::String(s) if s == "compat" => Profile::Compat,
                        other => return Err(format!("profile must be 'improved' or 'compat', not {other}")),
                    }
                }
                _ => return Err(format!("unknown option '{name}'; the options are {OPTION_NAMES}")),
            }
        }
        Ok(settings)
    }

    /// Attributes with these settings. The profile is set first: it re-derives the speed
    /// presets.
    pub fn attributes(&self) -> Result<Attributes, String> {
        let mut attr = Attributes::new();
        attr.set_profile(self.profile);
        let checked = (|| {
            attr.set_speed(self.speed)?;
            attr.set_max_colors(self.max_colors)?;
            if let Some((min, max)) = self.quality {
                attr.set_quality(min, max)?;
            }
            Ok::<_, libimgquant::Error>(())
        })();
        // parse() validated every value, so this only fails for hand-made Settings
        checked.map_err(|e| format!("invalid options: {e}"))?;
        Ok(attr)
    }
}

fn integer(value: &Value, min: u32, max: u32, name: &str) -> Result<u32, String> {
    match value {
        Value::Number(n) if n.fract() == 0.0 && (f64::from(min)..=f64::from(max)).contains(n) => Ok(*n as u32),
        other => Err(format!("{name} must be an integer from {min} to {max}, not {other}")),
    }
}

/// `{min, max}` (each optional: 0 and 100) or `[min, max]`.
fn quality(value: &Value) -> Result<(u8, u8), String> {
    const FORMS: &str = "quality must be {min, max} or [min, max] with integers from 0 to 100";
    let (min, max) = match value {
        Value::Array(items) => match items.as_slice() {
            [min, max] => (min, max),
            _ => return Err(format!("{FORMS}; the array has {} items", items.len())),
        },
        Value::Object(fields) => {
            let (mut min, mut max) = (&Value::Undefined, &Value::Undefined);
            for (name, value) in fields {
                match name.as_str() {
                    "min" => min = value,
                    "max" => max = value,
                    _ => return Err(format!("{FORMS}; unknown key '{name}'")),
                }
            }
            (min, max)
        }
        other => return Err(format!("{FORMS}, not {other}")),
    };
    let bound = |value: &Value, default: u8, name: &str| match value {
        Value::Undefined => Ok(default),
        value => integer(value, 0, 100, name).map(|v| v as u8),
    };
    let (min, max) = (bound(min, 0, "quality min")?, bound(max, 100, "quality max")?);
    if min > max {
        return Err(format!("quality min {min} is above max {max}"));
    }
    Ok((min, max))
}

fn dithering(value: &Value) -> Result<f32, String> {
    match value {
        Value::Number(n) if (0.0..=1.0).contains(n) => Ok(*n as f32),
        // true/false as full/none, the common case
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        other => Err(format!("dithering must be a number from 0 to 1, not {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(fields: &[(&str, Value)]) -> Value {
        Value::Object(fields.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect())
    }

    fn parse(fields: &[(&str, Value)]) -> Result<Settings, String> {
        Settings::parse(&object(fields))
    }

    fn n(v: f64) -> Value {
        Value::Number(v)
    }

    #[test]
    fn defaults() {
        let s = Settings::parse(&Value::Undefined).unwrap();
        assert_eq!(s, Settings::default());
        assert_eq!((s.max_colors, s.speed, s.quality, s.dithering), (256, 3, None, 1.0));
        assert_eq!(s.profile, Profile::Improved);
        assert_eq!(parse(&[]).unwrap(), s);
        // undefined (and null, which arrives as Undefined) leaves the default
        assert_eq!(parse(&[("speed", Value::Undefined)]).unwrap(), s);
    }

    #[test]
    fn all_options() {
        let s = parse(&[
            ("maxColors", n(64.0)),
            ("speed", n(10.0)),
            ("quality", object(&[("min", n(40.0)), ("max", n(80.0))])),
            ("dithering", n(0.5)),
            ("profile", Value::String("compat".into())),
        ])
        .unwrap();
        assert_eq!(
            s,
            Settings {
                max_colors: 64,
                speed: 10,
                quality: Some((40, 80)),
                dithering: 0.5,
                profile: Profile::Compat
            }
        );
        let attr = s.attributes().unwrap();
        assert_eq!((attr.max_colors(), attr.speed(), attr.quality()), (64, 10, (40, 80)));
        assert_eq!(attr.profile(), Profile::Compat);
    }

    #[test]
    fn quality_forms() {
        let q = |v: Value| parse(&[("quality", v)]).map(|s| s.quality.unwrap());
        assert_eq!(q(Value::Array(vec![n(65.0), n(90.0)])), Ok((65, 90)));
        assert_eq!(q(object(&[("min", n(65.0))])), Ok((65, 100)));
        assert_eq!(q(object(&[("max", n(80.0))])), Ok((0, 80)));
        assert_eq!(q(object(&[])), Ok((0, 100)));
        assert_eq!(q(Value::Array(vec![n(90.0), n(90.0)])), Ok((90, 90)));

        let err = |v: Value| q(v).unwrap_err();
        assert_eq!(
            err(Value::Array(vec![n(90.0), n(60.0)])),
            "quality min 90 is above max 60"
        );
        assert!(err(Value::Array(vec![n(90.0)])).ends_with("the array has 1 items"));
        assert!(err(object(&[("minimum", n(1.0))])).ends_with("unknown key 'minimum'"));
        assert_eq!(
            err(object(&[("max", n(101.0))])),
            "quality max must be an integer from 0 to 100, not 101"
        );
        assert!(err(n(80.0)).starts_with("quality must be {min, max} or [min, max]"));
        assert!(err(Value::Array(vec![n(-1.0), n(50.0)])).contains("not -1"));
        assert!(err(Value::Array(vec![n(10.5), n(50.0)])).contains("not 10.5"));
    }

    #[test]
    fn ranges() {
        let err = |name: &str, v: Value| parse(&[(name, v)]).unwrap_err();
        assert_eq!(
            err("maxColors", n(1.0)),
            "maxColors must be an integer from 2 to 256, not 1"
        );
        assert_eq!(
            err("maxColors", n(257.0)),
            "maxColors must be an integer from 2 to 256, not 257"
        );
        assert_eq!(
            err("maxColors", n(f64::NAN)),
            "maxColors must be an integer from 2 to 256, not NaN"
        );
        assert_eq!(
            err("maxColors", Value::String("16".into())),
            "maxColors must be an integer from 2 to 256, not '16'"
        );
        assert_eq!(err("speed", n(0.0)), "speed must be an integer from 1 to 10, not 0");
        assert_eq!(err("speed", n(2.5)), "speed must be an integer from 1 to 10, not 2.5");
        assert_eq!(
            err("speed", n(f64::INFINITY)),
            "speed must be an integer from 1 to 10, not inf"
        );
        assert_eq!(
            err("dithering", n(1.5)),
            "dithering must be a number from 0 to 1, not 1.5"
        );
        assert_eq!(
            err("dithering", n(-0.1)),
            "dithering must be a number from 0 to 1, not -0.1"
        );
        assert_eq!(
            err("profile", Value::String("fast".into())),
            "profile must be 'improved' or 'compat', not 'fast'"
        );
        assert_eq!(parse(&[("maxColors", n(2.0))]).unwrap().max_colors, 2);
        assert_eq!(parse(&[("maxColors", n(256.0))]).unwrap().max_colors, 256);
        assert_eq!(parse(&[("dithering", n(0.0))]).unwrap().dithering, 0.0);
        assert_eq!(parse(&[("dithering", Value::Bool(false))]).unwrap().dithering, 0.0);
        assert_eq!(parse(&[("dithering", Value::Bool(true))]).unwrap().dithering, 1.0);
    }

    #[test]
    fn wrong_shapes() {
        assert_eq!(
            Settings::parse(&n(3.0)).unwrap_err(),
            "options must be an object, not 3"
        );
        assert_eq!(
            Settings::parse(&Value::Array(vec![])).unwrap_err(),
            "options must be an object, not an array"
        );
        assert_eq!(
            parse(&[("colors", n(16.0))]).unwrap_err(),
            "unknown option 'colors'; the options are maxColors, speed, quality, dithering and profile"
        );
    }
}

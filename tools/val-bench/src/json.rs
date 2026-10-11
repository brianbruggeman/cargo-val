use std::fmt::{Display, Write};

#[derive(Default)]
pub struct Object {
    fields: Vec<(&'static str, String)>,
}

impl Object {
    pub fn text(&mut self, key: &'static str, value: &str) -> &mut Self {
        self.fields.push((key, quote(value)));
        self
    }

    pub fn number(&mut self, key: &'static str, value: impl Display) -> &mut Self {
        self.fields.push((key, value.to_string()));
        self
    }

    pub fn flag(&mut self, key: &'static str, value: bool) -> &mut Self {
        self.fields.push((key, value.to_string()));
        self
    }

    pub fn optional_number(&mut self, key: &'static str, value: Option<impl Display>) -> &mut Self {
        let rendered = value.map_or_else(|| "null".to_string(), |present| present.to_string());
        self.fields.push((key, rendered));
        self
    }

    pub fn optional_text(&mut self, key: &'static str, value: Option<&str>) -> &mut Self {
        let rendered = value.map_or_else(|| "null".to_string(), quote);
        self.fields.push((key, rendered));
        self
    }

    pub fn render(&self) -> String {
        let body: Vec<String> = self
            .fields
            .iter()
            .map(|(key, value)| format!("{}:{value}", quote(key)))
            .collect();
        format!("{{{}}}", body.join(","))
    }
}

pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_escapes_quotes_newlines_and_control_bytes() {
        assert_eq!(quote("a\"b\n\u{1b}[0m"), "\"a\\\"b\\n\\u001b[0m\"");
    }

    #[test]
    fn optional_number_renders_null_when_absent() {
        let mut object = Object::default();
        object.optional_number("first_result_ns", Some(7_u64)).optional_number("first_output_ns", None::<u64>);
        assert_eq!(object.render(), "{\"first_result_ns\":7,\"first_output_ns\":null}");
    }

    #[test]
    fn object_renders_fields_in_insertion_order() {
        let mut object = Object::default();
        object.text("arm", "nextest").number("wall_ns", 42_u64).flag("quiet_box", false).optional_text("sha", None);
        assert_eq!(object.render(), "{\"arm\":\"nextest\",\"wall_ns\":42,\"quiet_box\":false,\"sha\":null}");
    }
}

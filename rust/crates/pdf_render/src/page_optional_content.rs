//! Default screen visibility for optional content (ISO 32000-1, 8.11).
//! State operators still execute inside hidden marked content; only paint is
//! suppressed. Environment-dependent usage categories are reported explicitly.
use super::*;

fn array(doc: &PdfDocument, value: Option<&PdfObject>) -> Vec<PdfObject> {
    match value.map(|v| doc.resolve_value(v)) {
        Some(PdfObject::Array(values)) => values,
        _ => Vec::new(),
    }
}

fn intents(doc: &PdfDocument, value: Option<&PdfObject>) -> Vec<String> {
    match value.map(|v| doc.resolve_value(v)) {
        Some(PdfObject::Name(name)) => vec![name],
        Some(PdfObject::Array(values)) => values
            .iter()
            .filter_map(|v| v.as_name().map(str::to_owned))
            .collect(),
        _ => vec!["View".into()],
    }
}

impl Renderer<'_> {
    pub(super) fn optional_visible(&mut self, object: &PdfObject, depth: usize) -> bool {
        self.optional_state(object, depth, &mut 1024)
            .unwrap_or(true)
    }

    // None means that a group's intent excludes it from visibility decisions.
    // In particular it must not turn an AnyOff/AllOff membership into hidden.
    fn optional_state(
        &mut self,
        object: &PdfObject,
        depth: usize,
        budget: &mut usize,
    ) -> Option<bool> {
        if depth > 32 || *budget == 0 {
            self.warn("optional-content expression exceeds renderer limit; visibility may differ");
            return Some(true);
        }
        *budget -= 1;
        let Some(group) = self.doc.resolve_dict(object).cloned() else {
            self.warn("optional-content property is unreadable; visibility may differ");
            return Some(true);
        };
        Some(match group.get("Type").and_then(PdfObject::as_name) {
            Some("OCG") => return self.group_visible(object, &group),
            Some("OCMD") => {
                if let Some(expression) = group.get("VE") {
                    return self.visibility_expression(expression, depth + 1, budget);
                }
                let groups = match group.get("OCGs").map(|v| self.doc.resolve_value(v)) {
                    Some(PdfObject::Array(values)) => values,
                    Some(_) => group.get("OCGs").cloned().into_iter().collect(),
                    None => Vec::new(),
                };
                let states: Vec<_> = groups
                    .iter()
                    .filter_map(|v| self.optional_state(v, depth + 1, budget))
                    .collect();
                if states.is_empty() {
                    return None;
                }
                match group
                    .get("P")
                    .and_then(PdfObject::as_name)
                    .unwrap_or("AnyOn")
                {
                    "AnyOn" => states.iter().any(|state| *state),
                    "AllOn" => states.iter().all(|state| *state),
                    "AnyOff" => states.iter().any(|state| !state),
                    "AllOff" => states.iter().all(|state| !state),
                    _ => {
                        self.warn(
                            "unsupported optional-content membership policy; visibility may differ",
                        );
                        true
                    }
                }
            }
            _ => {
                self.warn("unsupported optional-content property; visibility may differ");
                true
            }
        })
    }

    fn visibility_expression(
        &mut self,
        expression: &PdfObject,
        depth: usize,
        budget: &mut usize,
    ) -> Option<bool> {
        if depth > 32 || *budget == 0 {
            self.warn("optional-content expression exceeds renderer limit; visibility may differ");
            return Some(true);
        }
        *budget -= 1;
        let PdfObject::Array(values) = self.doc.resolve_value(expression) else {
            return self.optional_state(expression, depth + 1, budget);
        };
        if values.len() < 2 {
            self.warn("unsupported optional-content visibility expression; visibility may differ");
            return Some(true);
        }
        let states: Vec<_> = values[1..]
            .iter()
            .filter_map(|v| self.visibility_expression(v, depth + 1, budget))
            .collect();
        if states.is_empty() {
            return None;
        }
        Some(match values[0].as_name() {
            Some("And") => states.iter().all(|v| *v),
            Some("Or") => states.iter().any(|v| *v),
            Some("Not") if values.len() == 2 && states.len() == 1 => !states[0],
            _ => {
                self.warn(
                    "unsupported optional-content visibility expression; visibility may differ",
                );
                true
            }
        })
    }

    fn group_visible(&mut self, object: &PdfObject, group: &Dictionary) -> Option<bool> {
        // Content marked as a layer in a document with no layer
        // configuration is shown, as every reader does. It is what pdfTeX
        // produces when it includes a figure that had layers of its own, so
        // it is common and there is no other visibility to differ from.
        let Some(properties) = self
            .doc
            .catalog()
            .and_then(|c| c.get("OCProperties"))
            .and_then(|v| self.doc.resolve_dict(v))
        else {
            return Some(true);
        };
        let config = properties
            .get("D")
            .and_then(|v| self.doc.resolve_dict(v))
            .cloned()
            .unwrap_or_default();
        let config_intents = intents(self.doc, config.get("Intent"));
        let group_intents = intents(self.doc, group.get("Intent"));
        if config_intents.is_empty()
            || !config_intents.iter().any(|v| v == "All")
                && !group_intents.iter().any(|v| config_intents.contains(v))
        {
            return None;
        }
        let base = config
            .get("BaseState")
            .and_then(PdfObject::as_name)
            .unwrap_or("ON");
        let mut visible = match base {
            "OFF" => array(self.doc, config.get("ON")).contains(object),
            "ON" => !array(self.doc, config.get("OFF")).contains(object),
            _ => {
                self.warn("undefined default optional-content state; visibility may differ");
                true
            }
        };
        let mut recommended = None;
        let mut unsupported_usage = false;
        for application in array(self.doc, config.get("AS")) {
            let Some(application) = self.doc.resolve_dict(&application).cloned() else {
                continue;
            };
            if application.get("Event").and_then(PdfObject::as_name) != Some("View") {
                continue;
            }
            let groups = array(self.doc, application.get("OCGs"));
            if !groups.contains(object) {
                continue;
            }
            for category in array(self.doc, application.get("Category")) {
                if category.as_name() == Some("View") {
                    let view = group
                        .get("Usage")
                        .and_then(|v| self.doc.resolve_dict(v))
                        .and_then(|d| d.get("View"))
                        .and_then(|v| self.doc.resolve_dict(v))
                        .and_then(|d| d.get("ViewState"))
                        .and_then(PdfObject::as_name);
                    if let Some(view) = view {
                        recommended = Some(recommended.unwrap_or(true) && view != "OFF");
                    }
                } else {
                    unsupported_usage = true;
                }
            }
        }
        if unsupported_usage {
            self.warn("environment-dependent optional-content visibility is unsupported; default layer state is shown");
        } else if let Some(recommended) = recommended {
            visible = recommended;
        }
        Some(visible)
    }
}

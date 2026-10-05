//! Import page-associated document structures without retaining discarded pages.
//!
//! Widgets, fields and outlines are graphs, not page-local resources. Build a
//! pruned source graph before copying it, then join its catalog entries to the
//! destination. This also keeps shared fields shared within a source document.
use std::collections::{BTreeMap, BTreeSet};

use pdf_core::document::PdfDocument;
use pdf_core::error::{PdfError, Result};
use pdf_core::object::{Dictionary, ObjectId, PdfObject};

use crate::metadata::decode_text_string;
use crate::page_tree::effective_page_dict;
use crate::split::copy_objects_into;

fn dict(doc: &PdfDocument, value: &PdfObject) -> Dictionary {
    doc.resolve_dict(value).cloned().unwrap_or_default()
}
fn array(doc: &PdfDocument, value: Option<&PdfObject>) -> Vec<PdfObject> {
    match value.map(|v| doc.resolve_value(v)) {
        Some(PdfObject::Array(values)) => values,
        _ => Vec::new(),
    }
}
fn string_bytes(value: &PdfObject) -> Option<&[u8]> {
    match value {
        PdfObject::LiteralString(v) | PdfObject::HexString(v) => Some(v),
        PdfObject::Name(v) => Some(v.as_bytes()),
        _ => None,
    }
}
fn indirect(doc: &mut PdfDocument, value: PdfObject) -> ObjectId {
    value.as_ref().unwrap_or_else(|| doc.add_object(value))
}
fn set_catalog(doc: &mut PdfDocument, key: &str, value: PdfObject) {
    let id = doc.root_ref().expect("target catalog installed");
    let mut catalog = doc.catalog().unwrap().clone();
    catalog.insert(key.into(), value);
    doc.set_object(id, PdfObject::Dictionary(catalog));
}

/// A unique set of source pages, in output order. The caller imports repeated
/// pages separately so their widgets can be edited independently.
pub(crate) fn import_pages(
    target: &mut PdfDocument,
    source: &PdfDocument,
    pages: &[ObjectId],
    batch: usize,
) -> Result<Vec<ObjectId>> {
    let mut working = source.clone();
    let all_pages = source.collect_page_ids().unwrap_or_default();
    let kept: BTreeSet<_> = pages.iter().copied().collect();
    let names = named_destinations(source);
    let mut widgets = BTreeMap::new();
    for &page in pages {
        let mut page_dict = effective_page_dict(source, page)?;
        page_dict.remove("Parent");
        if page_dict.contains_key("Annots") {
            let mut annots = Vec::new();
            for value in array(source, page_dict.get("Annots")) {
                let mut annotation = dict(source, &value);
                if annotation.is_empty() {
                    continue;
                }
                let id = indirect(&mut working, value);
                annotation.insert("P".into(), PdfObject::Reference(page));
                if annotation.get("Subtype").and_then(PdfObject::as_name) == Some("Widget") {
                    widgets.insert(id, page);
                }
                fix_destination_entries(&mut annotation, source, &names, &kept);
                working.set_object(id, PdfObject::Dictionary(annotation));
                annots.push(PdfObject::Reference(id));
            }
            page_dict.insert("Annots".into(), PdfObject::Array(annots));
        }
        working.set_object(page, PdfObject::Dictionary(page_dict));
    }
    // Prevent back references in unusual annotations/actions from resurrecting
    // pages that were not requested, or pulling in the original page tree.
    let page_tree_ids: Vec<_> = working
        .objects
        .iter()
        .filter_map(|(id, object)| {
            (object
                .value
                .as_dict()
                .and_then(|d| d.get("Type"))
                .and_then(PdfObject::as_name)
                == Some("Pages"))
            .then_some(*id)
        })
        .collect();
    for id in all_pages
        .iter()
        .filter(|id| !kept.contains(id))
        .chain(page_tree_ids.iter())
    {
        working.set_object(*id, PdfObject::Null);
    }
    let mut extras = Dictionary::new();
    if let Some(properties) = source.catalog().and_then(|c| c.get("OCProperties")) {
        // Include the catalog root in the same copy graph as the pages. This
        // makes /Properties, XObject /OC and configuration references point
        // to the very same remapped groups, including membership dictionaries.
        extras.insert("OCProperties".into(), properties.clone());
    }
    if let Some(form) = prepare_form(&mut working, source, &widgets, batch)? {
        extras.insert("AcroForm".into(), PdfObject::Dictionary(form));
    }
    if let Some(outlines) = prepare_outlines(&mut working, source, &names, &kept) {
        extras.insert("Outlines".into(), PdfObject::Reference(outlines));
    }
    let extra_id = working.add_object(PdfObject::Dictionary(extras));
    let mut roots: Vec<_> = pages
        .iter()
        .map(|id| (*id, working.resolve(*id).unwrap().clone()))
        .collect();
    roots.push((extra_id, working.resolve(extra_id).unwrap().clone()));
    let mut copied = copy_objects_into(target, &working, &roots)?;
    let imported = target
        .resolve(copied.pop().unwrap())
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    if let Some(form) = imported.get("AcroForm") {
        merge_form(target, dict(target, form));
    }
    if let Some(outlines) = imported.get("Outlines") {
        merge_outlines(target, outlines);
    }
    if let Some(properties) = imported.get("OCProperties") {
        merge_optional_content(target, properties)?;
    }
    Ok(copied)
}

fn merge_optional_content(target: &mut PdfDocument, incoming: &PdfObject) -> Result<()> {
    let incoming = target.resolve_dict(incoming).cloned().ok_or_else(|| {
        PdfError::Structure("optional-content properties must be a dictionary".into())
    })?;
    let Some(existing) = target
        .catalog()
        .and_then(|c| c.get("OCProperties"))
        .cloned()
    else {
        // Extraction and the first layered input retain every configuration.
        set_catalog(target, "OCProperties", PdfObject::Dictionary(incoming));
        return Ok(());
    };
    let mut existing = dict(target, &existing);
    if !array(target, existing.get("Configs")).is_empty()
        || !array(target, incoming.get("Configs")).is_empty()
    {
        return Err(PdfError::Structure(
            "cannot safely combine layered PDFs with alternate layer configurations; extract pages without duplication or use inputs with only a default layer configuration".into(),
        ));
    }
    let left = existing
        .get("D")
        .map(|v| dict(target, v))
        .unwrap_or_default();
    let right = incoming
        .get("D")
        .map(|v| dict(target, v))
        .unwrap_or_default();
    let intent = |config: &Dictionary| {
        config
            .get("Intent")
            .map(|v| target.resolve_value(v))
            .unwrap_or_else(|| PdfObject::Name("View".into()))
    };
    if intent(&left) != intent(&right) {
        return Err(PdfError::Structure(
            "cannot safely combine layered PDFs with different layer intents".into(),
        ));
    }
    let mut combined = left.clone();
    let mut off = Vec::new();
    for (properties, config) in [(&existing, &left), (&incoming, &right)] {
        let base = config
            .get("BaseState")
            .and_then(PdfObject::as_name)
            .unwrap_or("ON");
        if !matches!(base, "ON" | "OFF") {
            return Err(PdfError::Structure(
                "cannot safely combine an undefined base layer state".into(),
            ));
        }
        if config.keys().any(|key| {
            !matches!(
                key.as_str(),
                "Name"
                    | "Creator"
                    | "BaseState"
                    | "ON"
                    | "OFF"
                    | "Intent"
                    | "AS"
                    | "Order"
                    | "ListMode"
                    | "RBGroups"
                    | "Locked"
            )
        }) {
            return Err(PdfError::Structure(
                "cannot safely combine an unsupported layer configuration".into(),
            ));
        }
        let on_groups = array(target, config.get("ON"));
        let off_groups = array(target, config.get("OFF"));
        for group in array(target, properties.get("OCGs")) {
            let visible = if base == "OFF" {
                on_groups.contains(&group)
            } else {
                !off_groups.contains(&group)
            };
            if !visible {
                off.push(group);
            }
        }
    }
    combined.insert("BaseState".into(), PdfObject::Name("ON".into()));
    combined.remove("ON");
    combined.insert("OFF".into(), PdfObject::Array(off));
    for key in ["AS", "Order", "RBGroups", "Locked"] {
        let mut values = array(target, left.get(key));
        values.extend(array(target, right.get(key)));
        if !values.is_empty() {
            combined.insert(key.into(), PdfObject::Array(values));
        }
    }
    let mut groups = array(target, existing.get("OCGs"));
    groups.extend(array(target, incoming.get("OCGs")));
    existing.insert("OCGs".into(), PdfObject::Array(groups));
    existing.insert("D".into(), PdfObject::Dictionary(combined));
    set_catalog(target, "OCProperties", PdfObject::Dictionary(existing));
    Ok(())
}

fn prepare_form(
    working: &mut PdfDocument,
    source: &PdfDocument,
    widgets: &BTreeMap<ObjectId, ObjectId>,
    batch: usize,
) -> Result<Option<Dictionary>> {
    let Some(value) = source.catalog().and_then(|c| c.get("AcroForm")) else {
        return Ok(None);
    };
    let mut form = dict(source, value);
    if form.contains_key("XFA") {
        return Err(PdfError::Structure(
            "XFA forms cannot be safely split or merged; export an AcroForm PDF first".into(),
        ));
    }
    let mut visited = BTreeSet::new();
    let mut fields = Vec::new();
    for value in array(source, form.get("Fields")) {
        if let Some(id) = prune_field(working, value, None, widgets, &mut visited, 0) {
            fields.push(PdfObject::Reference(id));
        }
    }
    // Some writers omit widgets from /Fields. Retain them as editable roots.
    for &id in widgets.keys() {
        if !visited.contains(&id) {
            // Follow /Parent before pruning, otherwise turning a widget into a
            // root discards its inherited field type, value and appearance.
            let mut root = id;
            let mut ancestors = BTreeSet::new();
            while ancestors.len() < 128 && ancestors.insert(root) {
                let parent = working
                    .resolve(root)
                    .and_then(PdfObject::as_dict)
                    .and_then(|d| d.get("Parent"))
                    .and_then(PdfObject::as_ref);
                match parent {
                    Some(parent) if !ancestors.contains(&parent) => root = parent,
                    _ => break,
                }
            }
            if let Some(root) = prune_field(
                working,
                PdfObject::Reference(root),
                None,
                widgets,
                &mut visited,
                0,
            ) {
                fields.push(PdfObject::Reference(root));
            }
        }
    }
    if fields.is_empty() {
        return Ok(None);
    }
    // /DR belongs to the form, not to fields. Namespace its resource names so
    // equally named fonts from different inputs keep the correct glyphs.
    let mut renamed = BTreeMap::new();
    let mut resources = dict(source, form.get("DR").unwrap_or(&PdfObject::Null));
    for value in resources.values_mut() {
        let category = dict(source, value);
        if category.is_empty() {
            continue;
        }
        let mut new_category = Dictionary::new();
        for (name, resource) in category {
            let new_name = format!("D{batch}_{name}");
            renamed.insert(name, new_name.clone());
            new_category.insert(new_name, resource);
        }
        *value = PdfObject::Dictionary(new_category);
    }
    let default_da = form.get("DA").map(|v| source.resolve_value(v));
    for &id in &visited {
        let Some(mut field) = working.resolve(id).and_then(PdfObject::as_dict).cloned() else {
            continue;
        };
        if field.get("Parent").is_none() {
            // Materialize form defaults at each root before forms are joined.
            if !field.contains_key("DA") {
                if let Some(da) = &default_da {
                    field.insert("DA".into(), da.clone());
                }
            }
            if !field.contains_key("Q") {
                if let Some(q) = form.get("Q") {
                    field.insert("Q".into(), q.clone());
                }
            }
        }
        if let Some(da) = field.get("DA").map(|v| working.resolve_value(v)) {
            if let Some(bytes) = string_bytes(&da) {
                field.insert(
                    "DA".into(),
                    PdfObject::LiteralString(rename_da(bytes, &renamed)),
                );
            }
        }
        working.set_object(id, PdfObject::Dictionary(field));
    }
    form.insert("Fields".into(), PdfObject::Array(fields));
    form.insert("DR".into(), PdfObject::Dictionary(resources));
    form.remove("DA");
    form.remove("Q");
    let calculation_order = array(source, form.get("CO"))
        .into_iter()
        .filter(|v| v.as_ref().is_some_and(|id| visited.contains(&id)))
        .collect();
    form.insert("CO".into(), PdfObject::Array(calculation_order));
    Ok(Some(form))
}

fn prune_field(
    doc: &mut PdfDocument,
    value: PdfObject,
    parent: Option<ObjectId>,
    widgets: &BTreeMap<ObjectId, ObjectId>,
    visited: &mut BTreeSet<ObjectId>,
    depth: usize,
) -> Option<ObjectId> {
    if depth > 128 {
        return None;
    }
    let id = indirect(doc, value);
    if !visited.insert(id) {
        return None;
    }
    let mut field = doc.resolve(id)?.as_dict()?.clone();
    let is_widget = field.get("Subtype").and_then(PdfObject::as_name) == Some("Widget");
    if is_widget && !widgets.contains_key(&id) {
        visited.remove(&id);
        return None;
    }
    if let Some(page) = widgets.get(&id) {
        field.insert("P".into(), PdfObject::Reference(*page));
    }
    if field.contains_key("Kids") {
        let original_kids = array(doc, field.get("Kids"));
        let mut kept_indices = Vec::new();
        let kids = original_kids
            .iter()
            .enumerate()
            .filter_map(|(index, kid)| {
                let child = prune_field(doc, kid.clone(), Some(id), widgets, visited, depth + 1)?;
                kept_indices.push(index);
                Some(PdfObject::Reference(child))
            })
            .collect::<Vec<_>>();
        if kids.is_empty() && !is_widget {
            visited.remove(&id);
            return None;
        }
        if kids.len() != original_kids.len() {
            prune_button_options(doc, &mut field, &kids, &kept_indices, original_kids.len());
        }
        field.insert("Kids".into(), PdfObject::Array(kids));
    }
    if let Some(parent) = parent {
        field.insert("Parent".into(), PdfObject::Reference(parent));
    } else {
        field.remove("Parent");
    }
    doc.set_object(id, PdfObject::Dictionary(field));
    Some(id)
}

fn inherited_field_value(doc: &PdfDocument, field: &Dictionary, key: &str) -> Option<PdfObject> {
    let mut current = field.clone();
    let mut seen = BTreeSet::new();
    loop {
        if let Some(value) = current.get(key) {
            return Some(doc.resolve_value(value));
        }
        let parent = current.get("Parent")?.as_ref()?;
        if seen.len() >= 128 || !seen.insert(parent) {
            return None;
        }
        current = dict(doc, &PdfObject::Reference(parent));
    }
}

// A button's /Opt entries correspond to /Kids, unlike a choice field's /Opt.
// Keep their export values aligned when widgets on other pages are removed.
fn prune_button_options(
    doc: &mut PdfDocument,
    field: &mut Dictionary,
    kids: &[PdfObject],
    kept: &[usize],
    old_count: usize,
) {
    if inherited_field_value(doc, field, "FT")
        .as_ref()
        .and_then(PdfObject::as_name)
        != Some("Btn")
    {
        return;
    }
    let Some(PdfObject::Array(options)) = inherited_field_value(doc, field, "Opt") else {
        return;
    };
    if options.len() != old_count {
        return;
    }
    field.insert(
        "Opt".into(),
        PdfObject::Array(kept.iter().map(|&i| options[i].clone()).collect()),
    );
    for key in ["V", "DV"] {
        if let Some(PdfObject::Name(name)) = inherited_field_value(doc, field, key) {
            if let Ok(index) = name.parse::<usize>() {
                if index < old_count {
                    let value = kept
                        .iter()
                        .position(|&old| old == index)
                        .map(|new| new.to_string())
                        .unwrap_or_else(|| "Off".into());
                    field.insert(key.into(), PdfObject::Name(value));
                }
            }
        }
    }
    for (new_index, (kid, old_index)) in kids.iter().zip(kept).enumerate() {
        let old_name = old_index.to_string();
        let new_name = new_index.to_string();
        if old_name == new_name {
            continue;
        }
        let mut widget = dict(doc, kid);
        if widget.get("AS").and_then(PdfObject::as_name) == Some(old_name.as_str()) {
            widget.insert("AS".into(), PdfObject::Name(new_name.clone()));
        }
        if let Some(value) = widget.get("AP") {
            let mut appearances = dict(doc, value);
            for mode in ["N", "R", "D"] {
                if let Some(value) = appearances.get(mode) {
                    let mut states = dict(doc, value);
                    if let Some(appearance) = states.remove(&old_name) {
                        states.insert(new_name.clone(), appearance);
                        appearances.insert(mode.into(), PdfObject::Dictionary(states));
                    }
                }
            }
            widget.insert("AP".into(), PdfObject::Dictionary(appearances));
        }
        doc.set_object(kid.as_ref().unwrap(), PdfObject::Dictionary(widget));
    }
}

// Default appearance strings contain text/colour operators, not text strings.
// Preserve bytes and delimiters; decode PDF #xx names before renaming them.
fn rename_da(bytes: &[u8], names: &BTreeMap<String, String>) -> Vec<u8> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'/' {
            result.push(bytes[i]);
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && !b"()<>[]{}/%".contains(&bytes[i])
        {
            i += 1;
        }
        let raw = &bytes[start + 1..i];
        let mut decoded = Vec::new();
        let mut j = 0;
        while j < raw.len() {
            if raw[j] == b'#' && j + 2 < raw.len() {
                if let Ok(n) = u8::from_str_radix(&String::from_utf8_lossy(&raw[j + 1..j + 3]), 16)
                {
                    decoded.push(n);
                    j += 3;
                    continue;
                }
            }
            decoded.push(raw[j]);
            j += 1;
        }
        if let Some(name) = names.get(String::from_utf8_lossy(&decoded).as_ref()) {
            result.push(b'/');
            for byte in name.bytes() {
                if byte.is_ascii_whitespace()
                    || b"()<>[]{}/%#".contains(&byte)
                    || !(33..=126).contains(&byte)
                {
                    result.extend_from_slice(format!("#{byte:02X}").as_bytes());
                } else {
                    result.push(byte);
                }
            }
        } else {
            result.extend_from_slice(&bytes[start..i]);
        }
    }
    result
}

fn merge_form(target: &mut PdfDocument, imported: Dictionary) {
    let mut form = target
        .catalog()
        .and_then(|c| c.get("AcroForm"))
        .map(|v| dict(target, v))
        .unwrap_or_default();
    let mut fields = array(target, form.get("Fields"));
    let mut used_names = BTreeSet::new();
    for field in &fields {
        collect_field_names(target, field, "", &mut used_names, &mut BTreeSet::new(), 0);
    }
    for field in array(target, imported.get("Fields")) {
        let id = field.as_ref().unwrap();
        let mut data = dict(target, &field);
        let name = data
            .get("T")
            .and_then(string_bytes)
            .map(Vec::from)
            .unwrap_or_else(|| b"Field".to_vec());
        let mut suffix = 2;
        loop {
            let mut names = BTreeSet::new();
            collect_field_names(target, &field, "", &mut names, &mut BTreeSet::new(), 0);
            if names.is_disjoint(&used_names) {
                used_names.extend(names);
                break;
            }
            let mut unique = name.clone();
            // Preserve the original string encoding, including UTF-16 BE.
            if name.starts_with(&[0xfe, 0xff]) {
                for unit in format!("_{suffix}").encode_utf16() {
                    unique.extend(unit.to_be_bytes());
                }
            } else {
                unique.extend(format!("_{suffix}").as_bytes());
            }
            suffix += 1;
            data.insert("T".into(), PdfObject::LiteralString(unique));
            target.set_object(id, PdfObject::Dictionary(data.clone()));
        }
        fields.push(field);
    }
    form.insert("Fields".into(), PdfObject::Array(fields));
    let mut resources = dict(target, form.get("DR").unwrap_or(&PdfObject::Null));
    for (kind, entries) in dict(target, imported.get("DR").unwrap_or(&PdfObject::Null)) {
        let mut category = dict(target, resources.get(&kind).unwrap_or(&PdfObject::Null));
        category.extend(dict(target, &entries));
        resources.insert(kind, PdfObject::Dictionary(category));
    }
    form.insert("DR".into(), PdfObject::Dictionary(resources));
    let mut order = array(target, form.get("CO"));
    order.extend(array(target, imported.get("CO")));
    form.insert("CO".into(), PdfObject::Array(order));
    if imported.get("NeedAppearances") == Some(&PdfObject::Bool(true)) {
        form.insert("NeedAppearances".into(), PdfObject::Bool(true));
    }
    let flags = form
        .get("SigFlags")
        .and_then(PdfObject::as_i64)
        .unwrap_or(0)
        | imported
            .get("SigFlags")
            .and_then(PdfObject::as_i64)
            .unwrap_or(0);
    if flags != 0 {
        form.insert("SigFlags".into(), PdfObject::Integer(flags));
    }
    set_catalog(target, "AcroForm", PdfObject::Dictionary(form));
}

// Field identity is the fully qualified name, not the bytes of the root /T.
// Unnamed grouping nodes and mixed ASCII/UTF-16 encodings occur in real forms.
fn collect_field_names(
    doc: &PdfDocument,
    value: &PdfObject,
    prefix: &str,
    names: &mut BTreeSet<String>,
    seen: &mut BTreeSet<ObjectId>,
    depth: usize,
) {
    if depth > 128 || value.as_ref().is_some_and(|id| !seen.insert(id)) {
        return;
    }
    let field = dict(doc, value);
    let mut qualified = prefix.to_owned();
    if let Some(name) = field.get("T").map(|value| doc.resolve_value(value)) {
        if let Some(bytes) = string_bytes(&name) {
            if !qualified.is_empty() {
                qualified.push('.');
            }
            qualified.push_str(&decode_text_string(bytes));
            names.insert(qualified.clone());
        }
    }
    for kid in array(doc, field.get("Kids")) {
        collect_field_names(doc, &kid, &qualified, names, seen, depth + 1);
    }
}

fn named_destinations(source: &PdfDocument) -> BTreeMap<Vec<u8>, PdfObject> {
    let mut result = BTreeMap::new();
    let Some(catalog) = source.catalog() else {
        return result;
    };
    for (name, value) in dict(source, catalog.get("Dests").unwrap_or(&PdfObject::Null)) {
        result.insert(name.into_bytes(), value);
    }
    let names = dict(source, catalog.get("Names").unwrap_or(&PdfObject::Null));
    if let Some(root) = names.get("Dests") {
        collect_names(source, root, &mut result, &mut BTreeSet::new(), 0);
    }
    result
}
fn collect_names(
    source: &PdfDocument,
    value: &PdfObject,
    output: &mut BTreeMap<Vec<u8>, PdfObject>,
    seen: &mut BTreeSet<ObjectId>,
    depth: usize,
) {
    if depth > 128 || value.as_ref().is_some_and(|id| !seen.insert(id)) {
        return;
    }
    let data = dict(source, value);
    let names = array(source, data.get("Names"));
    for pair in names.chunks_exact(2) {
        if let Some(name) = string_bytes(&pair[0]) {
            output.insert(name.to_vec(), pair[1].clone());
        }
    }
    for kid in array(source, data.get("Kids")) {
        collect_names(source, &kid, output, seen, depth + 1);
    }
}
fn destination(
    source: &PdfDocument,
    value: &PdfObject,
    names: &BTreeMap<Vec<u8>, PdfObject>,
    kept: &BTreeSet<ObjectId>,
) -> Option<PdfObject> {
    let mut value = source.resolve_value(value);
    for _ in 0..32 {
        match &value {
            PdfObject::Array(items) => {
                return items
                    .first()
                    .and_then(PdfObject::as_ref)
                    .filter(|id| kept.contains(id))
                    .map(|_| value)
            }
            PdfObject::Dictionary(data) => value = source.resolve_value(data.get("D")?),
            _ => value = source.resolve_value(names.get(string_bytes(&value)?)?),
        }
    }
    None
}
fn fix_destination_entries(
    data: &mut Dictionary,
    source: &PdfDocument,
    names: &BTreeMap<Vec<u8>, PdfObject>,
    kept: &BTreeSet<ObjectId>,
) -> bool {
    let mut valid = true;
    if let Some(old) = data.remove("Dest") {
        if let Some(dest) = destination(source, &old, names, kept) {
            data.insert("Dest".into(), dest);
        } else {
            valid = false;
        }
    }
    if let Some(old) = data.get("A") {
        let mut action = dict(source, old);
        if action.get("S").and_then(PdfObject::as_name) == Some("GoTo") {
            if let Some(dest) = action
                .get("D")
                .and_then(|v| destination(source, v, names, kept))
            {
                action.insert("D".into(), dest);
                data.insert("A".into(), PdfObject::Dictionary(action));
            } else {
                data.remove("A");
                valid = false;
            }
        }
    }
    valid
}
struct Outline {
    data: Dictionary,
    children: Vec<Outline>,
    open: bool,
}
fn read_outlines(
    source: &PdfDocument,
    first: Option<ObjectId>,
    names: &BTreeMap<Vec<u8>, PdfObject>,
    kept: &BTreeSet<ObjectId>,
    seen: &mut BTreeSet<ObjectId>,
    depth: usize,
) -> Vec<Outline> {
    if depth > 128 {
        return Vec::new();
    }
    let mut result = Vec::new();
    let mut current = first;
    while let Some(id) = current {
        if !seen.insert(id) {
            break;
        }
        let Some(mut data) = source.resolve(id).and_then(PdfObject::as_dict).cloned() else {
            break;
        };
        current = data.get("Next").and_then(PdfObject::as_ref);
        let children = read_outlines(
            source,
            data.get("First").and_then(PdfObject::as_ref),
            names,
            kept,
            seen,
            depth + 1,
        );
        let open = data.get("Count").and_then(PdfObject::as_i64).unwrap_or(0) >= 0;
        let valid = fix_destination_entries(&mut data, source, names, kept);
        for key in ["Parent", "Prev", "Next", "First", "Last", "Count"] {
            data.remove(key);
        }
        if valid || !children.is_empty() {
            result.push(Outline {
                data,
                children,
                open,
            });
        }
    }
    result
}
fn write_outlines(
    doc: &mut PdfDocument,
    parent: ObjectId,
    nodes: Vec<Outline>,
) -> (Vec<ObjectId>, i64) {
    let ids: Vec<_> = nodes
        .iter()
        .map(|_| doc.add_object(PdfObject::Null))
        .collect();
    let mut visible = 0;
    for (index, node) in nodes.into_iter().enumerate() {
        let mut data = node.data;
        data.insert("Parent".into(), PdfObject::Reference(parent));
        if index > 0 {
            data.insert("Prev".into(), PdfObject::Reference(ids[index - 1]));
        }
        if index + 1 < ids.len() {
            data.insert("Next".into(), PdfObject::Reference(ids[index + 1]));
        }
        let (kids, count) = write_outlines(doc, ids[index], node.children);
        if let (Some(first), Some(last)) = (kids.first(), kids.last()) {
            data.insert("First".into(), PdfObject::Reference(*first));
            data.insert("Last".into(), PdfObject::Reference(*last));
            data.insert(
                "Count".into(),
                PdfObject::Integer(if node.open { count } else { -count }),
            );
        }
        visible += 1 + if node.open { count } else { 0 };
        doc.set_object(ids[index], PdfObject::Dictionary(data));
    }
    (ids, visible)
}
fn prepare_outlines(
    working: &mut PdfDocument,
    source: &PdfDocument,
    names: &BTreeMap<Vec<u8>, PdfObject>,
    kept: &BTreeSet<ObjectId>,
) -> Option<ObjectId> {
    let root = dict(source, source.catalog()?.get("Outlines")?);
    let nodes = read_outlines(
        source,
        root.get("First").and_then(PdfObject::as_ref),
        names,
        kept,
        &mut BTreeSet::new(),
        0,
    );
    if nodes.is_empty() {
        return None;
    }
    let id = working.add_object(PdfObject::Null);
    let (ids, count) = write_outlines(working, id, nodes);
    let mut root = Dictionary::new();
    root.insert("Type".into(), PdfObject::Name("Outlines".into()));
    root.insert("First".into(), PdfObject::Reference(ids[0]));
    root.insert("Last".into(), PdfObject::Reference(*ids.last().unwrap()));
    root.insert("Count".into(), PdfObject::Integer(count));
    working.set_object(id, PdfObject::Dictionary(root));
    Some(id)
}
fn merge_outlines(target: &mut PdfDocument, imported: &PdfObject) {
    let imported = dict(target, imported);
    let existing = target.catalog().and_then(|c| c.get("Outlines")).cloned();
    let id = existing
        .and_then(|v| v.as_ref())
        .unwrap_or_else(|| target.add_object(PdfObject::Dictionary(Dictionary::new())));
    let mut root = dict(target, &PdfObject::Reference(id));
    let first = imported["First"].as_ref().unwrap();
    let last = imported["Last"].as_ref().unwrap();
    let mut current = Some(first);
    while let Some(child) = current {
        let mut data = dict(target, &PdfObject::Reference(child));
        current = data.get("Next").and_then(PdfObject::as_ref);
        data.insert("Parent".into(), PdfObject::Reference(id));
        target.set_object(child, PdfObject::Dictionary(data));
    }
    if let Some(previous) = root.get("Last").and_then(PdfObject::as_ref) {
        let mut data = dict(target, &PdfObject::Reference(previous));
        data.insert("Next".into(), PdfObject::Reference(first));
        target.set_object(previous, PdfObject::Dictionary(data));
        let mut data = dict(target, &PdfObject::Reference(first));
        data.insert("Prev".into(), PdfObject::Reference(previous));
        target.set_object(first, PdfObject::Dictionary(data));
    } else {
        root.insert("First".into(), PdfObject::Reference(first));
    }
    root.insert("Last".into(), PdfObject::Reference(last));
    root.insert("Type".into(), PdfObject::Name("Outlines".into()));
    let count = root.get("Count").and_then(PdfObject::as_i64).unwrap_or(0)
        + imported["Count"].as_i64().unwrap_or(0);
    root.insert("Count".into(), PdfObject::Integer(count));
    target.set_object(id, PdfObject::Dictionary(root));
    set_catalog(target, "Outlines", PdfObject::Reference(id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::merge_documents;
    use crate::page_tree::test_support::nested_doc;
    use crate::split::{extract_pages, split_into_pages};
    use pdf_core::stream::PdfStream;

    fn named(value: &str) -> PdfObject {
        PdfObject::Name(value.into())
    }
    fn text(value: &str) -> PdfObject {
        PdfObject::LiteralString(value.as_bytes().to_vec())
    }
    fn dictionary(entries: &[(&str, PdfObject)]) -> PdfObject {
        PdfObject::Dictionary(
            entries
                .iter()
                .map(|(k, v)| ((*k).into(), v.clone()))
                .collect(),
        )
    }
    fn catalog_entry(doc: &mut PdfDocument, key: &str, value: PdfObject) {
        set_catalog(doc, key, value);
    }
    fn fixture(font: &str) -> PdfDocument {
        let mut doc = nested_doc(3);
        let pages = doc.collect_page_ids().unwrap();
        let font_id = doc.add_object(dictionary(&[
            ("Type", named("Font")),
            ("Subtype", named("Type1")),
            ("BaseFont", named(font)),
        ]));
        let appearance = doc.add_object(PdfObject::Stream(PdfStream {
            dictionary: [
                ("Type".into(), named("XObject")),
                ("Subtype".into(), named("Form")),
                (
                    "BBox".into(),
                    PdfObject::Array(
                        vec![0, 0, 100, 20]
                            .into_iter()
                            .map(PdfObject::Integer)
                            .collect(),
                    ),
                ),
            ]
            .into_iter()
            .collect(),
            data: b"0 0 100 20 re f".to_vec(),
        }));
        let parent = doc.add_object(PdfObject::Null);
        let mut widgets = Vec::new();
        for &page in &pages {
            let widget = doc.add_object(dictionary(&[
                ("Type", named("Annot")),
                ("Subtype", named("Widget")),
                ("Parent", PdfObject::Reference(parent)),
                ("P", PdfObject::Reference(page)),
                ("AP", dictionary(&[("N", PdfObject::Reference(appearance))])),
            ]));
            let mut page_dict = dict(&doc, &PdfObject::Reference(page));
            page_dict.insert(
                "Annots".into(),
                PdfObject::Array(vec![PdfObject::Reference(widget)]),
            );
            doc.set_object(page, PdfObject::Dictionary(page_dict));
            widgets.push(PdfObject::Reference(widget));
        }
        doc.set_object(
            parent,
            dictionary(&[
                ("FT", named("Tx")),
                ("T", text("Name")),
                ("V", text("Alice")),
                ("Kids", PdfObject::Array(widgets)),
            ]),
        );
        catalog_entry(
            &mut doc,
            "AcroForm",
            dictionary(&[
                (
                    "Fields",
                    PdfObject::Array(vec![PdfObject::Reference(parent)]),
                ),
                ("DA", text("/Helv 12 Tf 0 g")),
                ("Q", PdfObject::Integer(1)),
                (
                    "DR",
                    dictionary(&[(
                        "Font",
                        dictionary(&[("Helv", PdfObject::Reference(font_id))]),
                    )]),
                ),
                ("NeedAppearances", PdfObject::Bool(true)),
                ("CO", PdfObject::Array(vec![PdfObject::Reference(parent)])),
            ]),
        );
        let second_dest = PdfObject::Array(vec![PdfObject::Reference(pages[1]), named("Fit")]);
        let name_leaf = doc.add_object(dictionary(&[(
            "Names",
            PdfObject::Array(vec![text("second"), dictionary(&[("D", second_dest)])]),
        )]));
        catalog_entry(
            &mut doc,
            "Names",
            dictionary(&[(
                "Dests",
                dictionary(&[(
                    "Kids",
                    PdfObject::Array(vec![PdfObject::Reference(name_leaf)]),
                )]),
            )]),
        );
        let outline = doc.add_object(PdfObject::Null);
        let nodes = vec![
            Outline {
                data: [
                    ("Title".into(), text("First")),
                    (
                        "Dest".into(),
                        PdfObject::Array(vec![PdfObject::Reference(pages[0]), named("Fit")]),
                    ),
                ]
                .into_iter()
                .collect(),
                children: Vec::new(),
                open: true,
            },
            Outline {
                data: [
                    ("Title".into(), text("Section")),
                    (
                        "Dest".into(),
                        PdfObject::Array(vec![PdfObject::Reference(pages[0]), named("Fit")]),
                    ),
                ]
                .into_iter()
                .collect(),
                children: vec![Outline {
                    data: [
                        ("Title".into(), text("Second")),
                        (
                            "A".into(),
                            dictionary(&[("S", named("GoTo")), ("D", text("second"))]),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                    children: Vec::new(),
                    open: true,
                }],
                open: false,
            },
            Outline {
                data: [
                    ("Title".into(), text("Third")),
                    (
                        "Dest".into(),
                        PdfObject::Array(vec![PdfObject::Reference(pages[2]), named("Fit")]),
                    ),
                ]
                .into_iter()
                .collect(),
                children: Vec::new(),
                open: true,
            },
        ];
        let (ids, count) = write_outlines(&mut doc, outline, nodes);
        doc.set_object(
            outline,
            dictionary(&[
                ("Type", named("Outlines")),
                ("First", PdfObject::Reference(ids[0])),
                ("Last", PdfObject::Reference(ids[2])),
                ("Count", PdfObject::Integer(count)),
            ]),
        );
        catalog_entry(&mut doc, "Outlines", PdfObject::Reference(outline));
        // A named local link must continue working after the name tree goes away.
        let link = doc.add_object(dictionary(&[
            ("Subtype", named("Link")),
            ("Dest", text("second")),
        ]));
        let mut page = dict(&doc, &PdfObject::Reference(pages[1]));
        let mut annots = array(&doc, page.get("Annots"));
        annots.push(PdfObject::Reference(link));
        page.insert("Annots".into(), PdfObject::Array(annots));
        doc.set_object(pages[1], PdfObject::Dictionary(page));
        doc
    }
    fn round_trip(doc: PdfDocument) -> PdfDocument {
        PdfDocument::from_bytes(&doc.to_bytes().unwrap()).unwrap()
    }
    fn form(doc: &PdfDocument) -> Dictionary {
        dict(doc, &doc.catalog().unwrap()["AcroForm"])
    }
    fn assert_no_dangling(doc: &PdfDocument) {
        fn walk(doc: &PdfDocument, value: &PdfObject) {
            match value {
                PdfObject::Reference(id) => {
                    assert!(doc.objects.contains_key(id), "missing reference {id:?}")
                }
                PdfObject::Array(values) => values.iter().for_each(|v| walk(doc, v)),
                PdfObject::Dictionary(data) => data.values().for_each(|v| walk(doc, v)),
                PdfObject::Stream(stream) => stream.dictionary.values().for_each(|v| walk(doc, v)),
                _ => {}
            }
        }
        for object in doc.objects.values() {
            walk(doc, &object.value);
        }
    }
    #[test]
    fn split_preserves_fillable_field_tree_appearance_defaults_and_prunes_other_pages() {
        let source = fixture("Helvetica");
        for out in split_into_pages(&source).unwrap() {
            let out = round_trip(out);
            let pages = out.collect_page_ids().unwrap();
            let form = form(&out);
            let roots = array(&out, form.get("Fields"));
            assert_eq!(roots.len(), 1);
            let field = dict(&out, &roots[0]);
            assert_eq!(field["FT"], named("Tx"));
            assert_eq!(field["V"], text("Alice"));
            assert_eq!(field["DA"], text("/D0_Helv 12 Tf 0 g"));
            assert_eq!(field["Q"], PdfObject::Integer(1));
            assert!(!field.contains_key("Parent"));
            let kids = array(&out, field.get("Kids"));
            assert_eq!(kids.len(), 1);
            let widget = dict(&out, &kids[0]);
            assert_eq!(widget["Parent"], roots[0]);
            assert_eq!(widget["P"], PdfObject::Reference(pages[0]));
            let page = dict(&out, &PdfObject::Reference(pages[0]));
            assert_eq!(array(&out, page.get("Annots"))[0], kids[0]);
            let ap = dict(&out, &widget["AP"]);
            assert!(matches!(out.resolve_value(&ap["N"]), PdfObject::Stream(_)));
            let fonts = dict(&out, &dict(&out, &form["DR"])["Font"]);
            assert_eq!(
                dict(&out, &fonts["D0_Helv"])["BaseFont"],
                named("Helvetica")
            );
            assert_eq!(array(&out, form.get("CO")), roots);
            assert_eq!(
                out.objects
                    .values()
                    .filter(|o| o
                        .value
                        .as_dict()
                        .and_then(|d| d.get("Type"))
                        .and_then(PdfObject::as_name)
                        == Some("Page"))
                    .count(),
                1
            );
            assert_no_dangling(&out);
        }
    }
    #[test]
    fn subset_keeps_bookmark_hierarchy_remaps_named_destinations_and_local_links() {
        let out = round_trip(extract_pages(&fixture("Helvetica"), &[1]).unwrap());
        let page = out.collect_page_ids().unwrap()[0];
        let outline = dict(&out, &out.catalog().unwrap()["Outlines"]);
        let section = dict(&out, &outline["First"]);
        assert_eq!(section["Title"], text("Section"));
        assert!(!section.contains_key("Dest"));
        assert!(!section.contains_key("Next"));
        assert_eq!(section["Count"], PdfObject::Integer(-1));
        let child = dict(&out, &section["First"]);
        assert_eq!(child["Parent"], outline["First"]);
        let action = dict(&out, &child["A"]);
        assert_eq!(array(&out, action.get("D"))[0], PdfObject::Reference(page));
        let page_dict = dict(&out, &PdfObject::Reference(page));
        let annots = array(&out, page_dict.get("Annots"));
        let link = dict(&out, &annots[1]);
        assert_eq!(array(&out, link.get("Dest"))[0], PdfObject::Reference(page));
        assert_no_dangling(&out);
    }
    #[test]
    fn merge_combines_outlines_and_independent_same_named_fields_with_distinct_fonts() {
        let out =
            round_trip(merge_documents(&[&fixture("Helvetica"), &fixture("Courier")]).unwrap());
        let pages = out.collect_page_ids().unwrap();
        let form = form(&out);
        let fields = array(&out, form.get("Fields"));
        assert_eq!(fields.len(), 2);
        let first = dict(&out, &fields[0]);
        let second = dict(&out, &fields[1]);
        assert_eq!(first["T"], text("Name"));
        assert_eq!(second["T"], text("Name_2"));
        assert_eq!(second["DA"], text("/D1_Helv 12 Tf 0 g"));
        let fonts = dict(&out, &dict(&out, &form["DR"])["Font"]);
        assert_eq!(
            dict(&out, &fonts["D0_Helv"])["BaseFont"],
            named("Helvetica")
        );
        assert_eq!(dict(&out, &fonts["D1_Helv"])["BaseFont"], named("Courier"));
        for (offset, field) in [first, second].iter().enumerate() {
            for (index, widget) in array(&out, field.get("Kids")).iter().enumerate() {
                assert_eq!(
                    dict(&out, widget)["P"],
                    PdfObject::Reference(pages[offset * 3 + index])
                );
            }
        }
        let root_value = &out.catalog().unwrap()["Outlines"];
        let root = dict(&out, root_value);
        let mut next = root.get("First").and_then(PdfObject::as_ref);
        let mut previous = None;
        let mut count = 0;
        while let Some(id) = next {
            let node = dict(&out, &PdfObject::Reference(id));
            assert_eq!(&node["Parent"], root_value);
            assert_eq!(node.get("Prev").and_then(PdfObject::as_ref), previous);
            if count == 3 {
                assert_eq!(
                    array(&out, node.get("Dest"))[0],
                    PdfObject::Reference(pages[3])
                );
            }
            previous = Some(id);
            next = node.get("Next").and_then(PdfObject::as_ref);
            count += 1;
        }
        assert_eq!(count, 6);
        assert_eq!(root.get("Last").and_then(PdfObject::as_ref), previous);
        assert_no_dangling(&out);
    }
    #[test]
    fn duplicate_pages_have_independent_widgets_and_unique_editable_field_names() {
        let mut out = round_trip(extract_pages(&fixture("Helvetica"), &[1, 1]).unwrap());
        let pages = out.collect_page_ids().unwrap();
        assert_ne!(pages[0], pages[1]);
        let fields = array(&out, form(&out).get("Fields"));
        assert_eq!(fields.len(), 2);
        assert_ne!(dict(&out, &fields[0])["T"], dict(&out, &fields[1])["T"]);
        for (index, field) in fields.iter().enumerate() {
            let kids = array(&out, dict(&out, field).get("Kids"));
            assert_eq!(kids.len(), 1);
            assert_eq!(
                dict(&out, &kids[0])["P"],
                PdfObject::Reference(pages[index])
            );
        }
        let mut edited = dict(&out, &fields[1]);
        edited.insert("V".into(), text("Bob"));
        out.set_object(fields[1].as_ref().unwrap(), PdfObject::Dictionary(edited));
        let out = round_trip(out);
        assert_eq!(dict(&out, &fields[0])["V"], text("Alice"));
        assert_eq!(dict(&out, &fields[1])["V"], text("Bob"));
        assert_no_dangling(&out);
    }
    #[test]
    fn escaped_default_appearance_names_preserve_resource_identity() {
        let names = [("Font A".into(), "D0_Font A".into())]
            .into_iter()
            .collect();
        assert_eq!(
            rename_da(b"/Font#20A 12 Tf 0 g", &names),
            b"/D0_Font#20A 12 Tf 0 g"
        );
    }
    #[test]
    fn omitted_field_root_is_recovered_through_widget_parent_without_losing_values() {
        let mut source = fixture("Helvetica");
        let mut data = form(&source);
        data.insert("Fields".into(), PdfObject::Array(Vec::new()));
        catalog_entry(&mut source, "AcroForm", PdfObject::Dictionary(data));
        let out = round_trip(extract_pages(&source, &[1]).unwrap());
        let fields = array(&out, form(&out).get("Fields"));
        assert_eq!(fields.len(), 1);
        let field = dict(&out, &fields[0]);
        assert_eq!(field["T"], text("Name"));
        assert_eq!(field["FT"], named("Tx"));
        assert_eq!(field["V"], text("Alice"));
        let widgets = array(&out, field.get("Kids"));
        assert_eq!(widgets.len(), 1);
        assert_eq!(dict(&out, &widgets[0])["Parent"], fields[0]);
        assert_eq!(
            dict(&out, &widgets[0])["P"],
            PdfObject::Reference(out.collect_page_ids().unwrap()[0])
        );
        assert_no_dangling(&out);
    }
    #[test]
    fn merge_detects_field_name_collisions_across_string_encodings_and_unnamed_groups() {
        let source = fixture("Helvetica");
        let mut unicode = fixture("Courier");
        let field = array(&unicode, form(&unicode).get("Fields"))[0]
            .as_ref()
            .unwrap();
        let mut data = dict(&unicode, &PdfObject::Reference(field));
        let mut bytes = vec![0xfe, 0xff];
        for unit in "Name".encode_utf16() {
            bytes.extend(unit.to_be_bytes());
        }
        data.insert("T".into(), PdfObject::HexString(bytes));
        unicode.set_object(field, PdfObject::Dictionary(data));
        let mut grouped = fixture("Helvetica");
        let mut data = form(&grouped);
        let child = array(&grouped, data.get("Fields"))[0].as_ref().unwrap();
        let group = grouped.add_object(dictionary(&[(
            "Kids",
            PdfObject::Array(vec![PdfObject::Reference(child)]),
        )]));
        let mut child_data = dict(&grouped, &PdfObject::Reference(child));
        child_data.insert("Parent".into(), PdfObject::Reference(group));
        grouped.set_object(child, PdfObject::Dictionary(child_data));
        data.insert(
            "Fields".into(),
            PdfObject::Array(vec![PdfObject::Reference(group)]),
        );
        catalog_entry(&mut grouped, "AcroForm", PdfObject::Dictionary(data));
        let out = round_trip(merge_documents(&[&source, &unicode, &grouped]).unwrap());
        let fields = array(&out, form(&out).get("Fields"));
        assert_eq!(fields.len(), 3);
        assert_eq!(dict(&out, &fields[0])["T"], text("Name"));
        assert_eq!(
            decode_text_string(string_bytes(&dict(&out, &fields[1])["T"]).unwrap()),
            "Name_2"
        );
        assert_eq!(dict(&out, &fields[2])["T"], text("Field_2"));
        let mut all_names = BTreeSet::new();
        for field in fields {
            let mut names = BTreeSet::new();
            collect_field_names(&out, &field, "", &mut names, &mut BTreeSet::new(), 0);
            assert!(all_names.is_disjoint(&names));
            all_names.extend(names);
        }
        assert!(all_names.contains("Field_2.Name"));
        assert_no_dangling(&out);
    }
    #[test]
    fn subset_keeps_combined_checkbox_fields_and_appearance_states() {
        let mut source = nested_doc(2);
        let pages = source.collect_page_ids().unwrap();
        let appearance = source.add_object(PdfObject::Stream(PdfStream {
            dictionary: [
                ("Type".into(), named("XObject")),
                ("Subtype".into(), named("Form")),
                (
                    "BBox".into(),
                    PdfObject::Array(
                        vec![0, 0, 20, 20]
                            .into_iter()
                            .map(PdfObject::Integer)
                            .collect(),
                    ),
                ),
            ]
            .into_iter()
            .collect(),
            data: b"0 0 20 20 re f".to_vec(),
        }));
        let mut fields = Vec::new();
        for (index, page) in pages.iter().enumerate() {
            let widget = source.add_object(dictionary(&[
                ("Subtype", named("Widget")),
                ("FT", named("Btn")),
                ("T", text(&format!("Checkbox{index}"))),
                ("V", named("Yes")),
                ("AS", named("Yes")),
                ("P", PdfObject::Reference(*page)),
                (
                    "AP",
                    dictionary(&[(
                        "N",
                        dictionary(&[
                            ("Yes", PdfObject::Reference(appearance)),
                            ("Off", PdfObject::Reference(appearance)),
                        ]),
                    )]),
                ),
            ]));
            fields.push(PdfObject::Reference(widget));
            let mut data = dict(&source, &PdfObject::Reference(*page));
            data.insert(
                "Annots".into(),
                PdfObject::Array(vec![PdfObject::Reference(widget)]),
            );
            source.set_object(*page, PdfObject::Dictionary(data));
        }
        catalog_entry(
            &mut source,
            "AcroForm",
            dictionary(&[("Fields", PdfObject::Array(fields))]),
        );
        let out = round_trip(extract_pages(&source, &[1]).unwrap());
        let fields = array(&out, form(&out).get("Fields"));
        assert_eq!(fields.len(), 1);
        let checkbox = dict(&out, &fields[0]);
        assert_eq!(checkbox["T"], text("Checkbox1"));
        assert_eq!(checkbox["V"], named("Yes"));
        assert_eq!(checkbox["AS"], named("Yes"));
        assert!(!checkbox.contains_key("Parent"));
        let normal = dict(&out, &dict(&out, &checkbox["AP"])["N"]);
        assert!(matches!(
            out.resolve_value(&normal["Yes"]),
            PdfObject::Stream(_)
        ));
        assert!(normal.contains_key("Off"));
        assert_eq!(
            out.objects
                .values()
                .filter(|o| o
                    .value
                    .as_dict()
                    .and_then(|d| d.get("Type"))
                    .and_then(PdfObject::as_name)
                    == Some("Page"))
                .count(),
            1
        );
        assert_no_dangling(&out);
    }
    #[test]
    fn subset_keeps_radio_export_values_and_selected_appearance_indices_aligned() {
        let mut source = fixture("Helvetica");
        let root = array(&source, form(&source).get("Fields"))[0]
            .as_ref()
            .unwrap();
        let mut field = dict(&source, &PdfObject::Reference(root));
        let kids = array(&source, field.get("Kids"));
        for (index, kid) in kids.iter().enumerate() {
            let mut widget = dict(&source, kid);
            let appearance = dict(&source, &widget["AP"])["N"].clone();
            widget.insert(
                "AP".into(),
                dictionary(&[(
                    "N",
                    dictionary(&[
                        ("Off", appearance.clone()),
                        (&index.to_string(), appearance),
                    ]),
                )]),
            );
            widget.insert("AS".into(), named(if index == 1 { "1" } else { "Off" }));
            source.set_object(kid.as_ref().unwrap(), PdfObject::Dictionary(widget));
        }
        field.insert("FT".into(), named("Btn"));
        field.insert("Ff".into(), PdfObject::Integer(1 << 15));
        field.insert(
            "Opt".into(),
            PdfObject::Array(vec![text("First"), text("Second"), text("Third")]),
        );
        field.insert("V".into(), named("1"));
        field.insert("DV".into(), named("2"));
        source.set_object(root, PdfObject::Dictionary(field));
        let out = round_trip(extract_pages(&source, &[1]).unwrap());
        let field = dict(&out, &array(&out, form(&out).get("Fields"))[0]);
        assert_eq!(field["Opt"], PdfObject::Array(vec![text("Second")]));
        assert_eq!(field["V"], named("0"));
        assert_eq!(field["DV"], named("Off"));
        let widget = dict(&out, &array(&out, field.get("Kids"))[0]);
        assert_eq!(widget["AS"], named("0"));
        let states = dict(&out, &dict(&out, &widget["AP"])["N"]);
        assert!(states.contains_key("0"));
        assert!(states.contains_key("Off"));
        assert!(!states.contains_key("1"));
        assert_no_dangling(&out);
    }
    #[test]
    fn xfa_reports_an_explicit_error_instead_of_silently_discarding_form_data() {
        let mut source = fixture("Helvetica");
        let mut data = form(&source);
        data.insert("XFA".into(), text("<xfa/>"));
        catalog_entry(&mut source, "AcroForm", PdfObject::Dictionary(data));
        assert!(extract_pages(&source, &[0])
            .unwrap_err()
            .to_string()
            .contains("XFA"));
        assert!(merge_documents(&[&source])
            .unwrap_err()
            .to_string()
            .contains("XFA"));
    }
}

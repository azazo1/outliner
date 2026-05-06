use std::{collections::HashMap, path::Path};

use anyhow::{Context, Result, bail};
use qpdf::{QPdf, QPdfArray, QPdfDictionary, QPdfObject, QPdfObjectLike, QPdfObjectType};

use crate::model::{ExistingOutlineEntry, OutlineEntry};

pub fn open_pdf(path: &Path) -> Result<QPdf> {
    QPdf::read(path).with_context(|| format!("failed to open PDF {}", path.display()))
}

pub fn read_existing_outline(pdf: &QPdf) -> Result<Vec<ExistingOutlineEntry>> {
    let root = pdf.get_root().context("PDF root dictionary is missing")?;
    let Some(outlines) = root.get("/Outlines") else {
        return Ok(Vec::new());
    };

    let outline_dict: QPdfDictionary = outlines.into();
    let page_map = build_page_map(pdf)?;
    let named_destinations = build_named_destination_map(&root, &page_map);
    let mut entries = Vec::new();

    if let Some(first) = outline_dict.get("/First") {
        traverse_outline_items(
            &QPdfDictionary::from(first),
            1,
            &page_map,
            &named_destinations,
            &mut entries,
        )?;
    }

    Ok(entries)
}

pub fn write_outline(pdf: &QPdf, entries: &[OutlineEntry], output_path: &Path) -> Result<()> {
    if entries.is_empty() {
        bail!("cannot write an empty outline");
    }

    let root = pdf.get_root().context("PDF root dictionary is missing")?;
    let pages = pdf.get_pages().context("failed to load PDF pages")?;
    let count_object: QPdfObject = pdf.new_integer(entries.len() as i64).into();
    let outline_root: QPdfDictionary = pdf
        .new_dictionary_from(vec![("/Count", count_object)])
        .into_indirect()
        .into();

    let mut item_objects = Vec::with_capacity(entries.len());
    for entry in entries {
        let page = pages
            .get(entry.physical_page.saturating_sub(1))
            .with_context(|| {
                format!(
                    "outline target page {} is out of range",
                    entry.physical_page
                )
            })?;
        let dest = pdf.new_array_from(vec![page.as_ref().clone(), pdf.new_name("/Fit")]);
        let item: QPdfDictionary = pdf
            .new_dictionary_from([
                ("/Title", pdf.new_utf8_string(&entry.title)),
                ("/Dest", dest.into()),
            ])
            .into_indirect()
            .into();
        item_objects.push(item);
    }

    let tree = build_outline_tree(entries);

    if let Some(first_root) = tree.root_indices.first() {
        outline_root.set("/First", item_objects[*first_root].as_ref().clone());
    }
    if let Some(last_root) = tree.root_indices.last() {
        outline_root.set("/Last", item_objects[*last_root].as_ref().clone());
    }

    for (index, node) in tree.nodes.iter().enumerate() {
        let current = &item_objects[index];
        match node.parent {
            Some(parent) => current.set("/Parent", item_objects[parent].as_ref().clone()),
            None => current.set("/Parent", outline_root.as_ref().clone()),
        }

        if let Some(previous) = node.prev_sibling {
            current.set("/Prev", item_objects[previous].as_ref().clone());
        }
        if let Some(next) = node.next_sibling {
            current.set("/Next", item_objects[next].as_ref().clone());
        }
        if let Some(first_child) = node.children.first() {
            current.set("/First", item_objects[*first_child].as_ref().clone());
            current.set(
                "/Last",
                item_objects[*node.children.last().expect("child exists")]
                    .as_ref()
                    .clone(),
            );
            current.set("/Count", pdf.new_integer(node.descendant_count as i64));
        }
    }

    root.set("/Outlines", outline_root.as_ref().clone());
    root.set("/PageMode", pdf.new_name("/UseOutlines"));

    let mut writer = pdf.writer();
    writer.preserve_unreferenced_objects(false);
    writer.static_id(true);
    writer
        .write(output_path)
        .with_context(|| format!("failed to write outlined PDF to {}", output_path.display()))
}

fn build_page_map(pdf: &QPdf) -> Result<HashMap<(u32, u32), usize>> {
    let pages = pdf
        .get_pages()
        .context("failed to load pages while building page map")?;
    let mut page_map = HashMap::new();

    for (index, page) in pages.iter().enumerate() {
        page_map.insert((page.get_id(), page.get_generation()), index + 1);
    }

    Ok(page_map)
}

fn traverse_outline_items(
    item: &QPdfDictionary,
    level: usize,
    page_map: &HashMap<(u32, u32), usize>,
    named_destinations: &HashMap<String, usize>,
    entries: &mut Vec<ExistingOutlineEntry>,
) -> Result<()> {
    let mut current = Some(item.as_ref().clone());

    while let Some(object) = current {
        let dict: QPdfDictionary = object.into();
        let title = dict
            .get("/Title")
            .map(|title| title.as_string())
            .unwrap_or_default();
        let physical_page = resolve_outline_page(&dict, page_map, named_destinations);
        entries.push(ExistingOutlineEntry {
            title,
            level,
            physical_page,
        });

        if let Some(first_child) = dict.get("/First") {
            traverse_outline_items(
                &QPdfDictionary::from(first_child),
                level + 1,
                page_map,
                named_destinations,
                entries,
            )?;
        }

        current = dict.get("/Next");
    }

    Ok(())
}

fn resolve_outline_page(
    item: &QPdfDictionary,
    page_map: &HashMap<(u32, u32), usize>,
    named_destinations: &HashMap<String, usize>,
) -> Option<usize> {
    if let Some(dest) = item.get("/Dest") {
        return resolve_destination(dest, page_map, named_destinations);
    }

    let action = item.get("/A")?;
    let action_dict: QPdfDictionary = action.into();
    if action_dict.get("/S").map(|value| value.as_name()) != Some("/GoTo".to_string()) {
        return None;
    }

    let destination = action_dict.get("/D")?;
    resolve_destination(destination, page_map, named_destinations)
}

fn resolve_destination(
    destination: QPdfObject,
    page_map: &HashMap<(u32, u32), usize>,
    named_destinations: &HashMap<String, usize>,
) -> Option<usize> {
    match destination.get_type() {
        QPdfObjectType::Array => {
            let array: QPdfArray = destination.into();
            let first = array.get(0)?;
            resolve_page_reference(first, page_map)
        }
        QPdfObjectType::Name | QPdfObjectType::String => {
            let key = if destination.get_type() == QPdfObjectType::Name {
                destination.as_name()
            } else {
                destination.as_string()
            };
            named_destinations.get(&key).copied()
        }
        QPdfObjectType::Dictionary => {
            let dict: QPdfDictionary = destination.into();
            let nested = dict.get("/D")?;
            resolve_destination(nested, page_map, named_destinations)
        }
        _ => None,
    }
}

fn resolve_page_reference(
    page_object: QPdfObject,
    page_map: &HashMap<(u32, u32), usize>,
) -> Option<usize> {
    if page_object.is_indirect() {
        page_map
            .get(&(page_object.get_id(), page_object.get_generation()))
            .copied()
    } else if page_object.get_type() == QPdfObjectType::Integer {
        let scalar: qpdf::QPdfScalar = page_object.into();
        Some((scalar.as_i64() as usize) + 1)
    } else {
        None
    }
}

fn build_named_destination_map(
    root: &QPdfDictionary,
    page_map: &HashMap<(u32, u32), usize>,
) -> HashMap<String, usize> {
    let mut map = HashMap::new();

    if let Some(names) = root.get("/Names") {
        let names_dict: QPdfDictionary = names.into();
        if let Some(dests) = names_dict.get("/Dests") {
            collect_name_tree_destinations(&QPdfDictionary::from(dests), page_map, &mut map);
        }
    }

    if let Some(dests) = root.get("/Dests") {
        let dest_dict: QPdfDictionary = dests.into();
        for key in dest_dict.keys() {
            if let Some(value) = dest_dict.get(&key) {
                if let Some(page) = resolve_destination(value, page_map, &HashMap::new()) {
                    map.insert(key, page);
                }
            }
        }
    }

    map
}

fn collect_name_tree_destinations(
    dict: &QPdfDictionary,
    page_map: &HashMap<(u32, u32), usize>,
    map: &mut HashMap<String, usize>,
) {
    if let Some(names) = dict.get("/Names") {
        let names_array: QPdfArray = names.into();
        let mut index = 0usize;
        while index + 1 < names_array.len() {
            let key = names_array
                .get(index)
                .map(|value| {
                    if value.get_type() == QPdfObjectType::Name {
                        value.as_name()
                    } else {
                        value.as_string()
                    }
                })
                .unwrap_or_default();
            if let Some(value) = names_array.get(index + 1) {
                if let Some(page) = resolve_destination(value, page_map, &HashMap::new()) {
                    map.insert(key, page);
                }
            }
            index += 2;
        }
    }

    if let Some(kids) = dict.get("/Kids") {
        let kids_array: QPdfArray = kids.into();
        for child in kids_array.iter() {
            collect_name_tree_destinations(&QPdfDictionary::from(child), page_map, map);
        }
    }
}

struct OutlineTreeNode {
    parent: Option<usize>,
    children: Vec<usize>,
    prev_sibling: Option<usize>,
    next_sibling: Option<usize>,
    descendant_count: usize,
}

struct OutlineTree {
    nodes: Vec<OutlineTreeNode>,
    root_indices: Vec<usize>,
}

fn build_outline_tree(entries: &[OutlineEntry]) -> OutlineTree {
    let mut nodes = entries
        .iter()
        .map(|_| OutlineTreeNode {
            parent: None,
            children: Vec::new(),
            prev_sibling: None,
            next_sibling: None,
            descendant_count: 0,
        })
        .collect::<Vec<_>>();
    let mut root_indices = Vec::new();
    let mut stack: Vec<usize> = Vec::new();

    for (index, entry) in entries.iter().enumerate() {
        while stack.len() >= entry.level {
            stack.pop();
        }

        if let Some(&parent) = stack.last() {
            nodes[index].parent = Some(parent);
            nodes[parent].children.push(index);
        } else {
            root_indices.push(index);
        }

        stack.push(index);
    }

    let sibling_groups = std::iter::once(root_indices.clone())
        .chain(nodes.iter().map(|node| node.children.clone()))
        .collect::<Vec<_>>();

    for children in sibling_groups {
        for pair in children.windows(2) {
            let left = pair[0];
            let right = pair[1];
            nodes[left].next_sibling = Some(right);
            nodes[right].prev_sibling = Some(left);
        }
    }

    for root_index in &root_indices {
        update_descendant_counts(*root_index, &mut nodes);
    }

    OutlineTree {
        nodes,
        root_indices,
    }
}

fn update_descendant_counts(index: usize, nodes: &mut [OutlineTreeNode]) -> usize {
    let children = nodes[index].children.clone();
    let mut count = children.len();
    for child in children {
        count += update_descendant_counts(child, nodes);
    }
    nodes[index].descendant_count = count;
    count
}

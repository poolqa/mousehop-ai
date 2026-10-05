use std::{env, fs, path::PathBuf};

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xml_unescape(text: &str) -> String {
    let mut result = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        result.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find(';') else {
            break;
        };
        let entity = &rest[1..end];
        let ch = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .and_then(|n| u32::from_str_radix(n, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|n| n.parse().ok()))
                .and_then(char::from_u32),
        };
        if let Some(ch) = ch {
            result.push(ch);
        } else {
            result.push_str(&rest[..=end]);
        }
        rest = &rest[end + 1..];
    }
    result.push_str(rest);
    result
}

fn localize_template(template: &str, catalog: &str) -> String {
    let entries = catalog
        .lines()
        .map(|line| line.split_once('\t').expect("translation key and value"))
        .collect::<std::collections::HashMap<_, _>>();
    let mut output = String::new();
    let mut rest = template;
    while let Some(end) = rest.find('>') {
        output.push_str(&rest[..=end]);
        rest = &rest[end + 1..];
        let Some(start) = rest.find('<') else {
            break;
        };
        let text = &rest[..start];
        let decoded = xml_unescape(text);
        if let Some(value) = entries.get(decoded.as_str()) {
            output.push_str(&xml_escape(value));
        } else {
            output.push_str(text);
        }
        rest = &rest[start..];
    }
    output.push_str(rest);
    output
}

fn main() {
    glib_build_tools::compile_resources(
        &["resources"],
        "resources/resources.gresource.xml",
        "mousehop.gresource",
    );
    // Embed translated templates under the same resource paths. Register just
    // one bundle at startup, before any CompositeTemplate class is initialized.
    println!("cargo:rerun-if-changed=translations/zh-TW.tsv");
    let translated = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("zh-TW");
    fs::create_dir_all(&translated).unwrap();
    let catalog = fs::read_to_string("translations/zh-TW.tsv").unwrap();
    for entry in fs::read_dir("resources").unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "ui") {
            let template = localize_template(&fs::read_to_string(&path).unwrap(), &catalog);
            fs::write(translated.join(path.file_name().unwrap()), template).unwrap();
        }
    }
    glib_build_tools::compile_resources(
        &[translated.to_str().unwrap(), "resources"],
        "resources/resources.gresource.xml",
        "mousehop-zh-TW.gresource",
    );
}

use std::{env, fs, path::PathBuf};

#[path = "src/translation_catalog.rs"]
mod translation_catalog;

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
    let entries =
        translation_catalog::entries(catalog).collect::<std::collections::HashMap<_, _>>();
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
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let icon = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"))
            .join("../mousehop/assets/windows/app.ico");
        println!("cargo:rerun-if-changed={}", icon.display());
        let rc = PathBuf::from(env::var_os("OUT_DIR").expect("build directory")).join("ui.rc");
        fs::write(
            &rc,
            format!("1 ICON \"{}\"\n", icon.to_string_lossy().replace('\\', "/")),
        )
        .expect("UI icon resource");
        embed_resource::compile(rc, embed_resource::NONE)
            .manifest_required()
            .expect("compile UI icon");
    }
    shadow_rs::ShadowBuilder::builder()
        .deny_const(Default::default())
        .build()
        .expect("UI build metadata");
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

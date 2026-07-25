//! rusk-jnigen: the ".rs to Java bindings" half of the toolchain. It
//! scans a project's Rust source for `extern "C"`/`extern "system"`
//! functions named after the JNI mangling convention
//! (`Java_<package>_<Class>_<method>`) and produces the matching Java
//! `native` method declarations — so `pub extern "system" fn
//! Java_com_example_app_MainActivity_addNumbers(env: *mut JNIEnv, _class:
//! jclass, a: i32, b: i32) -> i32` on the Rust side gets you `public
//! static native int addNumbers(int a, int b);` on the Java side, without
//! writing (or hand-maintaining) either declaration twice.
//!
//! This is a lightweight textual scan, not a full `syn`-based parse: it
//! is intentionally forgiving about whitespace/attribute ordering, but it
//! only understands function signatures written in the straightforward
//! single-line-per-parameter style shown above (see [`scan_dir`] docs for
//! the exact shape it expects).

use std::path::{Path, PathBuf};

/// One recognized JNI export.
#[derive(Debug, Clone)]
pub struct JniFunction {
    pub class_name: String,
    pub method_name: String,
    pub java_return: String,
    /// (java type, parameter name)
    pub java_params: Vec<(String, String)>,
    pub source_file: PathBuf,
    /// Parameters whose Rust type wasn't recognized and were mapped to
    /// `Object` as a best-effort fallback — surfaced so `rusk build` can
    /// warn instead of silently generating a signature that won't link.
    pub unrecognized_types: Vec<String>,
}

/// Walks every `.rs` file under `src_dir` and extracts JNI exports whose
/// mangled name starts with `Java_<package_with_underscores>_`.
/// `package_id` is used to strip that known prefix unambiguously — JNI's
/// own mangling has no unambiguous package/class boundary without it,
/// since dots become underscores just like literal underscores in
/// identifiers do.
pub fn scan_dir(src_dir: &Path, package_id: &str) -> Vec<JniFunction> {
    let prefix = format!("Java_{}_", package_id.replace('.', "_"));
    let mut out = Vec::new();

    for entry in walkdir::WalkDir::new(src_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.path().extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        out.extend(scan_source(&text, &prefix, entry.path()));
    }
    out
}

fn scan_source(text: &str, prefix: &str, source_file: &Path) -> Vec<JniFunction> {
    let mut out = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel) = text[search_from..].find("fn Java_") {
        let fn_start = search_from + rel + 3; // position of "Java_..."
        search_from = fn_start + 1;

        let rest = &text[fn_start..];
        let name_end = rest
            .find(|c: char| c == '(' || c.is_whitespace())
            .unwrap_or(rest.len());
        let full_name = &rest[..name_end];
        let Some(mangled) = full_name.strip_prefix(prefix) else {
            continue;
        };
        // mangled == "<ClassName>_<methodName>" — take the last
        // underscore-delimited segment as the method, everything before
        // it as the class. Rust idents can't contain dots, so this is
        // unambiguous as long as neither name itself contains a literal
        // underscore (documented limitation — see module docs).
        let Some(underscore_idx) = mangled.rfind('_') else {
            continue;
        };
        let class_name = mangled[..underscore_idx].to_string();
        let method_name = mangled[underscore_idx + 1..].to_string();
        if class_name.is_empty() || method_name.is_empty() {
            continue;
        }

        let Some(paren_open) = rest.find('(') else {
            continue;
        };
        let Some(paren_close) = find_matching_paren(rest, paren_open) else {
            continue;
        };
        let params_text = &rest[paren_open + 1..paren_close];

        let after_params = &rest[paren_close + 1..];
        let body_start = after_params.find('{').unwrap_or(after_params.len());
        let signature_tail = &after_params[..body_start];
        let ret_rust = signature_tail
            .split("->")
            .nth(1)
            .map(|s| s.trim().to_string());

        let mut unrecognized = Vec::new();
        let all_params = split_top_level_commas(params_text);
        // Skip the leading JNIEnv + jclass/jobject parameters, which are
        // always present and never surfaced to Java.
        let user_params: Vec<&str> = all_params.iter().skip(2).map(|s| s.as_str()).collect();

        let mut java_params = Vec::new();
        for (i, param) in user_params.iter().enumerate() {
            let Some((name, rust_ty)) = param.split_once(':') else {
                continue;
            };
            let name = name.trim().trim_start_matches('_');
            let name = if name.is_empty() {
                format!("arg{i}")
            } else {
                name.to_string()
            };
            let rust_ty = rust_ty.trim();
            let java_ty = map_rust_type_to_java(rust_ty);
            if java_ty == "Object" && !is_known_object_type(rust_ty) {
                unrecognized.push(rust_ty.to_string());
            }
            java_params.push((java_ty.to_string(), name));
        }

        let java_return = ret_rust
            .as_deref()
            .map(map_rust_type_to_java)
            .unwrap_or("void")
            .to_string();

        out.push(JniFunction {
            class_name,
            method_name,
            java_return,
            java_params,
            source_file: source_file.to_path_buf(),
            unrecognized_types: unrecognized,
        });
    }
    out
}

fn find_matching_paren(s: &str, open_idx: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    for (i, &b) in bytes.iter().enumerate().skip(open_idx) {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    for c in s.chars() {
        match c {
            '<' | '(' | '[' => {
                depth += 1;
                current.push(c);
            }
            '>' | ')' | ']' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => {
                if !current.trim().is_empty() {
                    out.push(current.trim().to_string());
                }
                current.clear();
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn is_known_object_type(rust_ty: &str) -> bool {
    matches!(
        rust_ty,
        "jstring" | "jobject" | "jclass" | "jthrowable" | "jbyteArray" | "jintArray"
            | "jfloatArray" | "jdoubleArray" | "jlongArray" | "jbooleanArray" | "jobjectArray"
    )
}

fn map_rust_type_to_java(rust_ty: &str) -> &'static str {
    match rust_ty {
        "jint" | "i32" => "int",
        "jlong" | "i64" => "long",
        "jshort" | "i16" => "short",
        "jbyte" | "i8" => "byte",
        "jchar" | "u16" => "char",
        "jfloat" | "f32" => "float",
        "jdouble" | "f64" => "double",
        "jboolean" | "bool" => "boolean",
        "jstring" => "String",
        "jbyteArray" => "byte[]",
        "jintArray" => "int[]",
        "jfloatArray" => "float[]",
        "jdoubleArray" => "double[]",
        "jlongArray" => "long[]",
        "jbooleanArray" => "boolean[]",
        "jobjectArray" => "Object[]",
        "()" => "void",
        _ => "Object",
    }
}

/// Renders a single Java class (`class_name`, typically
/// `RuskNativeBindings`) declaring every scanned function as a
/// `public static native` method, grouped by the Rust source file they
/// came from. Nothing here is a fixed template — the whole body is built
/// from what [`scan_dir`] actually found.
pub fn generate_bindings_class(
    package: &str,
    class_name: &str,
    lib_name: &str,
    functions: &[JniFunction],
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "package {package};");
    let _ = writeln!(out);
    let _ = writeln!(out, "public final class {class_name} {{");
    let _ = writeln!(out, "    static {{");
    let _ = writeln!(out, "        System.loadLibrary(\"{lib_name}\");");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out);
    for f in functions {
        let params = f
            .java_params
            .iter()
            .map(|(ty, name)| format!("{ty} {name}"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            out,
            "    // generated from {} :: {}",
            f.source_file.display(),
            f.class_name
        );
        let _ = writeln!(
            out,
            "    public static native {} {}({params});",
            f.java_return, f.method_name
        );
    }
    let _ = writeln!(out, "}}");
    out
}

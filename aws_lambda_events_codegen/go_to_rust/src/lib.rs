#[macro_use]
extern crate log;
#[cfg(test)]
#[macro_use]
extern crate pest;
#[cfg(not(test))]
extern crate pest;
#[macro_use]
extern crate pest_derive;
extern crate codegen;
extern crate failure;
extern crate heck;
extern crate regex;
#[macro_use]
extern crate lazy_static;

use codegen::{Field, Scope, Struct};
use failure::Error;
use heck::{CamelCase, SnakeCase};
use pest::iterators::Pairs;
use pest::Parser;
use regex::Regex;
use std::boxed::Box;
use std::collections::HashSet;
use std::fmt;
use std::fs::File;
use std::io::prelude::*;
use std::path::PathBuf;

#[derive(Parser)]
#[grammar = "aws_go_events.pest"]
pub struct AwsGoEventsParser;

#[derive(Debug, Clone, PartialEq)]
pub struct GoCode(String);
impl fmt::Display for GoCode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
#[derive(Debug, Clone)]
pub struct RustCode(Scope);
impl RustCode {
    pub fn new(text: String) -> Self {
        RustCode(Scope::new().raw(&text).clone())
    }
    pub fn scope(&self) -> codegen::Scope {
        self.0.clone()
    }
    pub fn push_module(&mut self, m: codegen::Module) -> &mut Self {
        self.0.push_module(m);
        self
    }
}
impl fmt::Display for RustCode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0.to_string())
    }
}
impl PartialEq for RustCode {
    fn eq(&self, other: &RustCode) -> bool {
        self.0.to_string() == other.to_string()
    }
}

pub fn parse_go_file(path: &PathBuf) -> Result<(GoCode, RustCode), Error> {
    debug!("Parsing path: {:?}", &path.display());

    // Read the go code.
    let mut f = File::open(path)?;
    let mut go_code = String::new();
    f.read_to_string(&mut go_code)?;
    debug!("\n{}\n", go_code);

    // parse the go code into rust code.
    Ok(parse_go_string(go_code)?)
}

fn add_sorted_imports(scope: &mut Scope, libraries: &HashSet<String>) {
    // Stable sort the libraries.
    let mut ordered_libs: Vec<String> = libraries.iter().cloned().collect();
    ordered_libs.sort();

    // Import required libraries.
    for lib in ordered_libs {
        // Lame.
        let parts: Vec<&str> = lib.rsplitn(2, "::").collect();
        scope.import(parts[1], parts[0]);
    }
}

pub fn parse_go_string(go_source: String) -> Result<(GoCode, RustCode), Error> {
    let source = go_source.clone();

    let pairs = AwsGoEventsParser::parse(Rule::aws_go_events, &source.trim())
        .unwrap_or_else(|e| panic!("{}", e));

    let mut scope = Scope::new();

    for pair in pairs {
        match pair.as_rule() {
            Rule::struct_def => {
                let (parsed_struct, required_libraries) = parse_struct(pair.into_inner())?;
                scope.push_struct(parsed_struct);
                add_sorted_imports(&mut scope, &required_libraries);
            }
            Rule::type_alias => {
                let alias = parse_type_alias(pair.into_inner())?;
                if let Some((name, target)) = alias {
                    add_sorted_imports(&mut scope, &target.libraries);
                    // XXX: Add type definition support to `codegen`
                    for a in target.annotations {
                        scope.raw(&format!("#[{}]", a));
                    }
                    scope.raw(&format!("pub type {} = {};", name, target.value));
                }
            }
            // Skip some things for now.
            Rule::any_comment
            | Rule::constant_def
            | Rule::package_def
            | Rule::import
            | Rule::import_multiple
            | Rule::function
            | Rule::enum_options => {
                debug!("Skipping: {}", pair.clone().into_span().as_str());
                ()
            }
            _ => {
                panic!(
                    "Unexpected item at top-level:\n{:?}\n{}",
                    pair.clone(),
                    pair.clone().into_span().as_str()
                );
            }
        }
    }

    debug!("{}", &scope.to_string());

    /*
    let formatted_code =
        rustfmt_nightly::format_code_block(&scope.to_string(), &rustfmt_nightly::Config::default())
            .expect("formatted code");
    */

    Ok((GoCode(go_source), RustCode(scope)))
}

#[derive(Debug, Clone)]
struct FieldDef {
    name: String,
    json_name: Option<String>,
    comments: Vec<String>,
    omit_empty: bool,
    go_type: GoType,
    embedded: bool,
}

fn parse_comment(c: &str) -> String {
    c.replacen("//", "", 1).trim().to_string()
}

fn parse_type_alias(pairs: Pairs<Rule>) -> Result<Option<(String, RustType)>, Error> {
    debug!("Parsing type alias");
    let mut value = None;
    for pair in pairs {
        match pair.as_rule() {
            Rule::local_type_alias => {
                value = parse_local_type_alias(pair.into_inner())?;
            }
            Rule::package_type_alias => {
                value = parse_package_type_alias(pair.into_inner())?;
            }
            _ => unreachable!(),
        }
    }
    Ok(value)
}

fn parse_local_type_alias(pairs: Pairs<Rule>) -> Result<Option<(String, RustType)>, Error> {
    debug!("Parsing local type alias");
    let mut name: Option<String> = None;
    let mut target: Option<GoType> = None;

    for pair in pairs {
        let span = pair.clone().into_span();
        match pair.as_rule() {
            Rule::ident => name = Some(mangle(span.as_str())),
            Rule::type_alias_target => {
                target = Some(parse_go_type(pair.into_inner())?);
            }
            _ => unreachable!(),
        }
    }

    let name = name.expect("parsed name");
    let target = target.expect("parsed target");

    Ok(Some((name, translate_go_type_to_rust_type(target, None)?)))
}

fn parse_package_type_alias(pairs: Pairs<Rule>) -> Result<Option<(String, RustType)>, Error> {
    debug!("Parsing package type alias");
    let mut name: Option<String> = None;
    let mut target: Option<GoType> = None;

    for pair in pairs {
        let span = pair.clone().into_span();
        let value = span.as_str();
        match pair.as_rule() {
            Rule::ident => name = Some(mangle(span.as_str())),
            Rule::package_ident => {
                target = Some(parse_go_package_ident(value)?);
            }
            _ => unreachable!(),
        }
    }

    let name = name.expect("parsed name");
    let target = target.expect("parsed target");

    Ok(Some((name, translate_go_type_to_rust_type(target, None)?)))
}

fn parse_struct(pairs: Pairs<Rule>) -> Result<(codegen::Struct, HashSet<String>), Error> {
    debug!("Parsing struct");
    let mut name: Option<String> = None;
    let mut fields: Vec<FieldDef> = Vec::new();
    let mut comments: Vec<String> = Vec::new();

    for pair in pairs {
        let span = pair.clone().into_span();
        match pair.as_rule() {
            Rule::doc_comment => {
                comments.push(parse_comment(span.as_str()));
            }
            Rule::struct_preamble => {
                name = Some(parse_struct_preamble(pair.into_inner())?);
            }
            Rule::struct_fields => {
                fields = parse_struct_fields(pair.into_inner())?;
            }
            _ => unreachable!(),
        }
    }

    let struct_name = name.expect("parsed name");

    let mut rust_struct = Struct::new(&struct_name.to_camel_case());

    // Make it public.
    rust_struct.vis("pub");

    // Add some derives.
    rust_struct.derive("Debug");
    rust_struct.derive("Clone");
    rust_struct.derive("PartialEq");
    rust_struct.derive("Deserialize");
    rust_struct.derive("Serialize");

    if !comments.is_empty() {
        let annotated_comments: Vec<String> = comments
            .iter_mut()
            .map(|x| x.replace(&struct_name, &format!("`{}`", &struct_name.to_camel_case())))
            .collect();
        rust_struct.doc(&annotated_comments.join("\n"));
    }

    lazy_static! {
        static ref HASHMAP_RE: Regex = Regex::new("^HashMap<.+>$").expect("regex to compile");
    }

    let mut libraries: HashSet<String> = HashSet::new();

    let mut generics = 0;

    for f in fields {
        // Translate the name.
        let member_name = mangle(&f.name.to_snake_case());

        let mut rust_data = translate_go_type_to_rust_type(f.go_type, Some(&mut generics))?;
        let mut rust_type = rust_data.value;

        for generic in rust_data.generics {
            match generic.default {
                None => {
                    rust_struct.generic(&generic.value);
                }
                Some(default) => {
                    rust_struct.generic(format!("{}={}", generic.value, default).as_str());
                }
            }

            for bound in generic.bounds {
                rust_struct.bound(&generic.value, bound);
            }
        }

        // Extract the code and the libraries from the result.
        for lib in rust_data.libraries.iter() {
            libraries.insert(lib.clone());
        }

        // Make fields optional if they are optional in the json.
        if f.omit_empty {
            // We don't do this for HashMaps as they are handled special below.
            if !HASHMAP_RE.is_match(&rust_type) {
                rust_type = format!("Option<{}>", rust_type);
            }
        }

        if let Some(rename) = f.json_name.clone() {
            if rename != member_name {
                rust_data
                    .annotations
                    .push(format!("#[serde(rename = \"{}\")]", rename));
            }
        }

        if f.embedded {
            rust_data
                .annotations
                .push("#[serde(flatten)]".to_string());
        }

        let mut field_defs = vec![];

        // Behavior overrides for specific types.
        if rust_type == "String" {
            // Go converts null strings to "" and sometimes is wrong about
            // json string fields that can be `null`. We treat all `String`
            // fields as `Option<String>` and convert `""` to `None`.
            libraries.insert("custom_serde::*".to_string());

            let mut string_as_option = Field::new(&member_name, "Option<String>");
            string_as_option.annotation(vec![
                "#[serde(deserialize_with = \"deserialize_lambda_string\")]",
                "#[serde(default)]",
            ]);
            field_defs.push(string_as_option);
        } else if HASHMAP_RE.is_match(&rust_type) {
            // We default to an empty `HashMap` even if the field is `null`.
            libraries.insert("custom_serde::*".to_string());
            let mut map_as_empty = Field::new(&member_name, &rust_type);
            map_as_empty.annotation(vec![
                "#[serde(deserialize_with = \"deserialize_lambda_map\")]",
                "#[serde(default)]",
            ]);
            field_defs.push(map_as_empty);
        } else {
            field_defs = vec![Field::new(&member_name, &rust_type)];
        }

        for mut field in field_defs {
            // Fields are public.
            field.vis("pub");

            if !f.comments.is_empty() {
                field.doc(&f.comments.join("\n"));
            }

            if !rust_data.annotations.is_empty() {
                let mut all_annotations: Vec<String> = field.get_annotation();
                let mut new_annotations: Vec<String> = rust_data.annotations.clone();
                all_annotations.append(&mut new_annotations);
                field.annotation(all_annotations.iter().map(String::as_str).collect());
            }

            rust_struct.push_field(field);
        }
    }

    Ok((rust_struct, libraries))
}

fn parse_struct_preamble(pairs: Pairs<Rule>) -> Result<String, Error> {
    debug!("Parsing struct preamble");
    let mut name: Option<String> = None;

    for pair in pairs {
        let span = pair.clone().into_span();
        match pair.as_rule() {
            Rule::struct_name => {
                name = Some(span.as_str().to_string());
            }
            _ => unimplemented!(),
        }
    }

    Ok(name.expect("structs always have a name"))
}

fn parse_struct_fields(pairs: Pairs<Rule>) -> Result<Vec<FieldDef>, Error> {
    debug!("Parsing struct fields");

    let mut fields: Vec<FieldDef> = Vec::new();

    for pair in pairs {
        match pair.as_rule() {
            Rule::struct_field => fields.push(parse_struct_field(pair.into_inner())?),
            _ => unimplemented!(),
        }
    }

    Ok(fields)
}

fn parse_struct_field(pairs: Pairs<Rule>) -> Result<FieldDef, Error> {
    debug!("Parsing struct field");
    let mut name: Option<String> = None;
    let mut json: Option<JsonMapping> = None;
    let mut go_type: Option<GoType> = None;
    let mut comments: Vec<String> = vec![];
    let mut is_pointer = false;
    let mut embedded = false;

    for pair in pairs {
        debug!("{:?}", pair);
        let span = pair.clone().into_span();
        match pair.as_rule() {
            Rule::json_mapping => json = Some(parse_json_mapping(pair.into_inner())?),
            Rule::doc_comment => comments.push(parse_comment(span.as_str())),
            Rule::struct_field_decl => {
                for pair in pair.into_inner() {
                    let span = pair.clone().into_span();
                    match pair.as_rule() {
                        Rule::ident => name = Some(mangle(span.as_str())),
                        Rule::pointer => is_pointer = true,
                        Rule::struct_field_type => {
                            go_type = Some(parse_go_type(pair.into_inner())?)
                        }
                        Rule::struct_embedded_field => {
                            info!("struct_embedded_field found: {:?}", pair);
                            let value = pair.clone().into_span().as_str();
                            name = Some(mangle(value));
                            go_type = Some(parse_go_type(pair.into_inner())?);
                            embedded = true;
                        },
                        rule @ _ => panic!("invalid Rule found in struct_field_decl: {:?}", rule),
                    }
                }
            }
            _ => unimplemented!(),
        }
    }

    let json_name = if let Some(j) = json.clone() {
        Some(j.name)
    } else {
        None
    };

    let mut omit_empty = if let Some(j) = json.clone() {
        // We omit empty (aka use an Option) if the JSON says so.
        j.omit_empty
    } else {
        // By default we don't omit empty.
        false
    };

    if is_pointer {
        // If given a pointer, it can be `nil` and essentially empty.
        omit_empty = true
    }

    // Parse inline comment after json definition.
    if let Some(j) = json {
        if let Some(inline_comment) = j.comment {
            if !comments.is_empty() {
                // Append inline comment with a blank comment line before it.
                comments.push("".to_string());
            }
            comments.push(inline_comment)
        }
    };

    Ok(FieldDef {
        name: name.expect("fields have names"),
        json_name,
        comments,
        omit_empty,
        go_type: go_type.expect("fields have types"),
        embedded,
    })
}

#[derive(Debug, Clone)]
struct JsonMapping {
    name: String,
    comment: Option<String>,
    omit_empty: bool,
}

fn parse_json_mapping(pairs: Pairs<Rule>) -> Result<JsonMapping, Error> {
    debug!("Parsing json mapping");
    let mut name: Option<String> = None;
    let mut comment: Option<String> = None;
    let mut omit_empty = false;

    for pair in pairs {
        debug!("{:?}", pair);
        let span = pair.clone().into_span();
        match pair.as_rule() {
            Rule::json_name => name = Some(span.as_str().to_string()),
            Rule::any_comment => comment = Some(parse_comment(span.as_str())),
            Rule::omit_empty => omit_empty = true,
            _ => unimplemented!(),
        }
    }

    Ok(JsonMapping {
        name: name.expect("json mappings always have a name"),
        comment,
        omit_empty,
    })
}

#[derive(Debug, Clone)]
enum GoType {
    StringType,
    IntType,
    UnsignedIntType,
    FloatType,
    BoolType,
    ByteType,
    UserDefined(String),
    ArrayType(Box<GoType>),
    MapType(Box<GoType>, Box<GoType>),
    InterfaceType,
    PointerType(Box<GoType>),
    TimeType,
    TimestampMillisecondsType,
    TimestampSecondsType,
    JsonRawType,
}

struct RustType {
    annotations: Vec<String>,
    libraries: HashSet<String>,
    value: String,
    generics: Vec<RustGeneric>,
}

#[derive(Clone)]
struct RustGeneric {
    value: String,
    default: Option<String>,
    bounds: Vec<String>,
}

fn parse_go_type(pairs: Pairs<Rule>) -> Result<GoType, Error> {
    debug!("Parsing go type");
    let mut go_type: Option<GoType> = None;

    for pair in pairs {
        debug!("{:?}", pair);
        let value = pair.clone().into_span().as_str();
        go_type = match pair.as_rule() {
            Rule::array => Some(parse_go_type_array(pair.into_inner())?),
            Rule::primitive => Some(parse_go_type_primitive(value)?),
            Rule::ident => Some(parse_go_ident(value)?),
            Rule::package_ident => Some(parse_go_package_ident(value)?),
            Rule::map => Some(parse_go_type_map(pair.into_inner())?),
            Rule::interface => Some(parse_go_type_interface(value)?),
            Rule::pointer_type => Some(parse_go_type_pointer(pair.into_inner())?),
            _ => unimplemented!("{}\n{}", value, pair),
        };
    }

    Ok(go_type.expect("parsing go type"))
}

fn parse_go_type_array(pairs: Pairs<Rule>) -> Result<GoType, Error> {
    debug!("Parsing go array");
    let mut go_type: Option<GoType> = None;

    for pair in pairs {
        debug!("{:?}", pair);
        let value = pair.clone().into_span().as_str();
        go_type = match pair.as_rule() {
            Rule::primitive => Some(GoType::ArrayType(Box::new(parse_go_type_primitive(value)?))),
            Rule::ident => Some(GoType::ArrayType(Box::new(GoType::UserDefined(
                value.to_string(),
            )))),
            Rule::map => Some(GoType::ArrayType(Box::new(parse_go_type_map(
                pair.into_inner(),
            )?))),
            Rule::array => Some(GoType::ArrayType(Box::new(parse_go_type_array(
                pair.into_inner(),
            )?))),
            _ => unimplemented!(),
        };
    }

    Ok(go_type.expect("parsing go array"))
}

fn parse_go_type_map(pairs: Pairs<Rule>) -> Result<GoType, Error> {
    debug!("Parsing go map");
    let mut key_type: Option<GoType> = None;
    let mut value_type: Option<GoType> = None;

    for pair in pairs {
        debug!("{:?}", pair);
        let value = pair.clone().into_span().as_str();
        match pair.as_rule() {
            Rule::key_type => key_type = Some(parse_go_type_primitive(value)?),
            Rule::value_type => value_type = Some(parse_go_type(pair.into_inner())?),
            _ => unimplemented!(),
        };
    }

    Ok(GoType::MapType(
        Box::new(key_type.expect("parsing map key")),
        Box::new(value_type.expect("parsing map value")),
    ))
}

fn parse_go_type_interface(_t: &str) -> Result<GoType, Error> {
    // For now we don't parse.
    Ok(GoType::InterfaceType)
}

fn parse_go_type_pointer(pairs: Pairs<Rule>) -> Result<GoType, Error> {
    debug!("Parsing go pointer");
    let mut pointed_at = None;
    for pair in pairs {
        debug!("{:?}", pair);
        match pair.as_rule() {
            Rule::pointer => (),
            Rule::value_type => pointed_at = Some(parse_go_type(pair.into_inner())?),
            _ => unimplemented!(),
        };
    }
    Ok(GoType::PointerType(Box::new(pointed_at.expect("something pointed at"))))
}

fn parse_go_type_primitive(t: &str) -> Result<GoType, Error> {
    match t {
        "string" => Ok(GoType::StringType),
        "int" | "int32" | "int64" => Ok(GoType::IntType),
        "uint" | "uint32" | "uint64" => Ok(GoType::UnsignedIntType),
        "float" | "float32" | "float64" => Ok(GoType::FloatType),
        "bool" => Ok(GoType::BoolType),
        "byte" => Ok(GoType::ByteType),
        _ => unimplemented!("missing go type primitive"),
    }
}

fn parse_go_ident(t: &str) -> Result<GoType, Error> {
    match t {
        "MilliSecondsEpochTime" => Ok(GoType::TimestampMillisecondsType),
        "SecondsEpochTime" => Ok(GoType::TimestampSecondsType),
        _ => Ok(GoType::UserDefined(t.to_string())),
    }
}

fn parse_go_package_ident(t: &str) -> Result<GoType, Error> {
    match t {
        "time.Time" => Ok(GoType::TimeType),
        "json.RawMessage" => Ok(GoType::JsonRawType),
        _ => unimplemented!("missing go package ident mapping"),
    }
}

fn mangle(s: &str) -> String {
    // TODO: Add more keywords.
    match s {
        "ref" => "ref_".to_string(),
        "type" => "type_".to_string(),
        _ => s.to_string(),
    }
}

fn make_rust_type_with_no_libraries(value: &str) -> RustType {
    RustType {
        annotations: vec![],
        value: value.to_string(),
        generics: vec![],
        libraries: HashSet::new(),
    }
}

fn translate_go_type_to_rust_type(go_type: GoType, generic_counter: Option<&mut usize>) -> Result<RustType, Error> {
    let rust_type = match &go_type {
        GoType::StringType => make_rust_type_with_no_libraries("String"),
        GoType::BoolType => make_rust_type_with_no_libraries("bool"),
        GoType::ByteType => make_rust_type_with_no_libraries("u8"),
        GoType::IntType => make_rust_type_with_no_libraries("i64"),
        GoType::UnsignedIntType => make_rust_type_with_no_libraries("u64"),
        GoType::FloatType => make_rust_type_with_no_libraries("f64"),
        GoType::UserDefined(x) => make_rust_type_with_no_libraries(&x.to_camel_case()),
        GoType::ArrayType(x) => {
            let mut i = translate_go_type_to_rust_type(*x.clone(), generic_counter)?;
            
            if i.value == "u8" {
                let mut libraries = i.libraries.clone();
                libraries.insert("super::super::encodings::Base64Data".to_string());
                // Handle []u8 special, as it is base64 encoded.
                RustType {
                    annotations: i.annotations,
                    value: "Base64Data".to_string(),
                    generics: i.generics,
                    libraries: libraries,
                }
            } else {
                RustType {
                    annotations: i.annotations,
                    value: format!("Vec<{}>", i.value),
                    generics: i.generics,
                    libraries: i.libraries,
                }
            }
        },
        GoType::PointerType(v) => {
            let data = translate_go_type_to_rust_type(*v.clone(), generic_counter)?;
            let libraries: HashSet<String> = data.libraries.iter().cloned().collect();
            RustType {
                annotations: data.annotations,
                value: format!("Option<{}>", data.value),
                generics: data.generics,
                libraries,
            }
        },
        GoType::MapType(k, v) => {
            // TODO can we use a ref to the option to save this dance?
            let mut generics = 0;

            if let Some(ref generic_counter) = generic_counter {
                generics = **generic_counter;
            }

            let key_data = translate_go_type_to_rust_type(*k.clone(), Some(&mut generics))?;
            let value_data = translate_go_type_to_rust_type(*v.clone(), Some(&mut generics))?;

            if let Some(mut generic_counter) = generic_counter {
                *generic_counter = generics;
            }

            let mut annotations = Vec::new();
            annotations.extend(key_data.annotations);
            annotations.extend(value_data.annotations);

            let mut generics = Vec::new();
            generics.extend(key_data.generics);
            generics.extend(value_data.generics);

            let mut libraries = HashSet::new();
            libraries.extend(key_data.libraries);
            libraries.extend(value_data.libraries);
            libraries.insert("std::collections::HashMap".to_string());

            RustType {
                value: format!("HashMap<{}, {}>", key_data.value, value_data.value),
                annotations,
                generics,
                libraries,
            }
        }
        // For now we treat interfaces as a generic JSON value and make callers
        // deal with it.
        GoType::InterfaceType | GoType::JsonRawType => {
            let mut libraries = HashSet::new();
            libraries.insert("serde_json::Value".to_string());

            match generic_counter {
                Some(mut counter) => {
                    *counter = *counter + 1;
                    let next_generic = format!("T{}", counter);

                    libraries.insert("serde::de::DeserializeOwned".to_string());
                    libraries.insert("serde::ser::Serialize".to_string());

                    RustType {
                        annotations: vec!["#[serde(bound=\"\")]".to_string()],
                        value: next_generic.clone(),
                        generics: vec![RustGeneric {
                            value: next_generic.clone(),
                            default: Some("Value".to_string()),
                            bounds: vec![
                                "DeserializeOwned".to_string(),
                                "Serialize".to_string(),
                            ],
                        }],
                        libraries,
                    }
                }
                None => {
                    RustType {
                        annotations: vec![],
                        value: "Value".to_string(),
                        generics: vec![],
                        libraries,
                    }
                }
            }
        }
        GoType::TimestampSecondsType => {
            let mut libraries = HashSet::new();
            libraries.insert("super::super::encodings::SecondTimestamp".to_string());
            RustType {
                annotations: vec![],
                value: "SecondTimestamp".to_string(),
                generics: vec![],
                libraries,
            }
        }
        GoType::TimestampMillisecondsType => {
            let mut libraries = HashSet::new();
            libraries.insert("super::super::encodings::MillisecondTimestamp".to_string());

            RustType {
                annotations: vec![],
                value: "MillisecondTimestamp".to_string(),
                generics: vec![],
                libraries,
            }
        }
        GoType::TimeType => {
            // No need for custom deserialization as Go's time.Time type
            // deserializes to chrono's default format. Neat.
            let mut libraries = HashSet::new();
            libraries.insert("chrono::DateTime".to_string());
            libraries.insert("chrono::Utc".to_string());

            RustType {
                annotations: vec![],
                value: "DateTime<Utc>".to_string(),
                generics: vec![],
                libraries,
            }
        }
    };

    Ok(rust_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod primitives {
        use super::*;

        #[test]
        fn test_parses_array() {
            parses_to! {
                parser: AwsGoEventsParser,
                input: "[]bool",
                rule: Rule::array,
                tokens: [
                    array(0, 6, [
                        primitive(2, 6, [
                            boolean(2, 6),
                        ]),
                    ]),
                ]
            };

            parses_to! {
                parser: AwsGoEventsParser,
                input: "[]blah",
                rule: Rule::array,
                tokens: [
                    array(0, 6, [
                        ident(2, 6),
                    ]),
                ]
            };
        }

        #[test]
        fn test_parses_map() {
            parses_to! {
                parser: AwsGoEventsParser,
                input: "map[string]interface{}",
                rule: Rule::map,
                tokens: [
                    map(0, 22, [
                        key_type(4, 10, [
                            primitive(4, 10, [
                                string(4, 10),
                            ]),
                        ]),
                        value_type(11, 22, [
                            interface(11, 22),
                        ]),
                    ]),
                ]
            };
        }
    }

    mod directives {
        use super::*;

        #[test]
        fn test_parses_package_def() {
            parses_to! {
                parser: AwsGoEventsParser,
                input: "package foo",
                rule: Rule::package_def,
                tokens: [
                    package_def(0, 11, [
                        ident(8, 11),
                    ]),
                ]
            };
        }

        #[test]
        fn test_parses_struct_def() {
            parses_to! {
                parser: AwsGoEventsParser,
                input: "type MyFoo struct {}",
                rule: Rule::struct_def,
                tokens: [
                    struct_def(0, 20, [
                        struct_preamble(0, 17, [
                            struct_name(5, 10, [
                                ident(5, 10),
                            ]),
                        ]),
                    ]),
                ]
            };

            parses_to! {
                parser: AwsGoEventsParser,
                input: "type MyFoo struct { foo string }",
                rule: Rule::struct_def,
                tokens: [
                    struct_def(0, 32, [
                        struct_preamble(0, 17, [
                            struct_name(5, 10, [
                                ident(5, 10),
                            ]),
                        ]),
                        struct_fields(20, 31, [
                            struct_field(20, 31, [
                                struct_field_decl(20, 30, [
                                    ident(20, 23),
                                    struct_field_type(24, 30, [
                                        primitive(24, 30, [
                                            string(24, 30)
                                        ]),
                                    ]),
                                ]),
                            ]),
                        ]),
                    ]),
                ]
            };

            parses_to! {
                parser: AwsGoEventsParser,
                input: "type MyFoo struct { T1 }",
                rule: Rule::struct_def,
                tokens: [
                    struct_def(0, 24, [
                        struct_preamble(0, 17, [
                            struct_name(5, 10, [
                                ident(5, 10),
                            ]),
                        ]),
                        struct_fields(20, 23, [
                            struct_field(20, 23, [
                                struct_field_decl(20, 22, [
                                    struct_embedded_field(20, 22, [
                                        ident(20, 22),
                                    ]),
                                ]),
                            ]),
                        ]),
                    ]),
                ]
            };

            parses_to! {
                parser: AwsGoEventsParser,
                input: "type MyFoo struct { *T1 }",
                rule: Rule::struct_def,
                tokens: [
                    struct_def(0, 25, [
                        struct_preamble(0, 17, [
                            struct_name(5, 10, [
                                ident(5, 10),
                            ]),
                        ]),
                        struct_fields(20, 24, [
                            struct_field(20, 24, [
                                struct_field_decl(20, 23, [
                                    pointer(20, 21),
                                    struct_embedded_field(21, 23, [
                                        ident(21, 23),
                                    ]),
                                ]),
                            ]),
                        ]),
                    ]),
                ]
            };

            parses_to! {
                parser: AwsGoEventsParser,
                input: r#"type MyFoo struct {
                  foo string
                  bar int
                }"#,
                rule: Rule::struct_def,
                tokens: [
                    struct_def(0, 92, [
                        struct_preamble(0, 17, [
                            struct_name(5, 10, [
                                ident(5, 10),
                            ]),
                        ]),
                        struct_fields(38, 74, [
                            struct_field(38, 48, [
                                struct_field_decl(38, 48, [
                                    ident(38, 41),
                                    struct_field_type(42, 48, [
                                        primitive(42, 48, [
                                            string(42, 48)
                                        ]),
                                    ]),
                                ]),
                            ]),
                            struct_field(67, 74, [
                                struct_field_decl(67, 74, [
                                    ident(67, 70),
                                    struct_field_type(71, 74, [
                                        primitive(71, 74, [
                                            int(71, 74)
                                        ]),
                                    ]),
                                ]),
                            ]),
                        ]),
                    ]),
                ]
            };
        }

        #[test]
        fn test_parses_struct_field() {
            parses_to! {
                parser: AwsGoEventsParser,
                input: "EventVersion string",
                rule: Rule::struct_field,
                tokens: [
                    struct_field(0, 19, [
                        struct_field_decl(0, 19, [
                            ident(0, 12),
                            struct_field_type(13, 19, [
                                primitive(13, 19, [
                                    string(13, 19),
                                ]),
                            ]),
                        ]),
                    ]),
                ]
            };

            parses_to! {
                parser: AwsGoEventsParser,
                input: "EventVersion bool",
                rule: Rule::struct_field,
                tokens: [
                    struct_field(0, 17, [
                        struct_field_decl(0, 17, [
                            ident(0, 12),
                            struct_field_type(13, 17, [
                                primitive(13, 17, [
                                    boolean(13, 17),
                                ]),
                            ]),
                        ]),
                    ]),
                ]
            };

            parses_to! {
                parser: AwsGoEventsParser,
                input: "EventVersion *bool",
                rule: Rule::struct_field,
                tokens: [
                    struct_field(0, 18, [
                        struct_field_decl(0, 18, [
                            ident(0, 12),
                            pointer(13, 14),
                            struct_field_type(14, 18, [
                                primitive(14, 18, [
                                    boolean(14, 18),
                                ]),
                            ]),
                        ]),
                    ]),
                ]
            };
        }

        #[test]
        fn test_parses_struct_embedded_field() {
            parses_to! {
                parser: AwsGoEventsParser,
                input: "EventVersion",
                rule: Rule::struct_field,
                tokens: [
                    struct_field(0, 12, [
                        struct_field_decl(0, 12, [
                            struct_embedded_field(0, 12, [
                                ident(0, 12),
                            ]),
                        ]),
                    ]),
                ]
            };

            parses_to! {
                parser: AwsGoEventsParser,
                input: "*EventVersion",
                rule: Rule::struct_field,
                tokens: [
                    struct_field(0, 13, [
                        struct_field_decl(0, 13, [
                            pointer(0, 1),
                            struct_embedded_field(1, 13, [
                                ident(1, 13),
                            ]),
                        ]),
                    ]),
                ]
            };

            parses_to! {
                parser: AwsGoEventsParser,
                input: "EventVersion MyType",
                rule: Rule::struct_field,
                tokens: [
                    struct_field(0, 19, [
                        struct_field_decl(0, 19, [
                            ident(0, 12),
                            struct_field_type(13, 19, [
                                ident(13, 19),
                            ]),
                        ]),
                    ]),
                ]
            };
        }

        #[test]
        fn test_parses_json_mapping() {
            parses_to! {
                parser: AwsGoEventsParser,
                input: "`json:\"fooBar\"`",
                rule: Rule::json_mapping,
                tokens: [
                    json_mapping(0, 15, [
                        json_name(7, 13),
                    ]),
                ]
            };

            parses_to! {
                parser: AwsGoEventsParser,
                input: "`json:\"foo-x\"`",
                rule: Rule::json_mapping,
                tokens: [
                    json_mapping(0, 14, [
                        json_name(7, 12),
                    ]),
                ]
            };

            parses_to! {
                parser: AwsGoEventsParser,
                input: "`json:\"foo.x\"`",
                rule: Rule::json_mapping,
                tokens: [
                    json_mapping(0, 14, [
                        json_name(7, 12),
                    ]),
                ]
            };

            parses_to! {
                parser: AwsGoEventsParser,
                input: "`json:\"foo,omitempty\"`",
                rule: Rule::json_mapping,
                tokens: [
                    json_mapping(0, 22, [
                        json_name(7, 10),
                        omit_empty(10, 20),
                    ]),
                ]
            };

            parses_to! {
                parser: AwsGoEventsParser,
                input: "`json:\"fooBar\"` // whatever",
                rule: Rule::json_mapping,
                tokens: [
                    json_mapping(0, 27, [
                        json_name(7, 13),
                        any_comment(16, 27),
                    ]),
                ]
            };
        }

        #[test]
        fn test_parses_import() {
            parses_to! {
                parser: AwsGoEventsParser,
                input: "import \"foo\"",
                rule: Rule::import,
                tokens: [
                    import(0, 12, [
                        import_package(7, 12, [
                            package_name(8, 11),
                        ]),
                    ]),
                ]
            };

            parses_to! {
                parser: AwsGoEventsParser,
                input: "import \"a/b\"",
                rule: Rule::import,
                tokens: [
                    import(0, 12, [
                        import_package(7, 12, [
                            package_name(8, 11),
                        ]),
                    ]),
                ]
            };
        }

        #[test]
        fn test_parses_mutiple_imports() {
            parses_to! {
                parser: AwsGoEventsParser,
                input: "import (\n\"foo\"\n \"bar\"\n)",
                rule: Rule::import_multiple,
                tokens: [
                    import_multiple(0, 23, [
                        import_package(9, 14, [
                            package_name(10, 13),
                        ]),
                        import_package(16, 21, [
                            package_name(17, 20),
                        ]),
                    ]),
                ]
            };
        }
    }
}

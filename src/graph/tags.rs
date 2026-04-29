use std::collections::HashMap;

use tree_sitter::{Language, Query, QueryCursor, StreamingIterator};

/// A symbol extracted via tree-sitter tags query.
pub struct TaggedSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub start_line: usize,
    pub end_line: usize,
    pub start_col: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Interface,
    Trait,
    Impl,
    Module,
    Macro,
    Constant,
    Type,
}

impl SymbolKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Class => "class",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Interface => "interface",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::Module => "module",
            Self::Macro => "macro",
            Self::Constant => "const",
            Self::Type => "type",
        }
    }
}

/// A reference extracted via tree-sitter tags query.
pub struct TaggedRef {
    pub name: String,
    pub kind: RefKind,
    pub start_line: usize,
}

#[derive(Debug, Clone, Copy)]
pub enum RefKind {
    Call,
    Implementation,
}

/// Extract symbols and references from source code using tags.scm queries.
pub fn extract_tags(
    code: &[u8],
    lang: &str,
    ts_lang: Language,
    tree: &tree_sitter::Tree,
) -> (Vec<TaggedSymbol>, Vec<TaggedRef>) {
    let query_src = match tags_query(lang) {
        Some(q) => q,
        None => return (vec![], vec![]),
    };

    // Strip predicates that tree-sitter 0.25 query API doesn't support
    let clean_query = strip_unsupported_predicates(query_src);

    let query = match Query::new(&ts_lang, &clean_query) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("  warning: tags query failed for {lang}: {e}");
            return (vec![], vec![]);
        }
    };

    let capture_names: Vec<&str> = query.capture_names().iter().map(|s| s.as_ref()).collect();

    // Build capture index map
    let name_idx = capture_names.iter().position(|n| *n == "name");
    let doc_idx = capture_names.iter().position(|n| *n == "doc");

    // Map definition/reference capture names to their indices
    let mut def_captures: HashMap<u32, SymbolKind> = HashMap::new();
    let mut ref_captures: HashMap<u32, RefKind> = HashMap::new();

    for (i, name) in capture_names.iter().enumerate() {
        let idx = i as u32;
        match *name {
            "definition.function" => {
                def_captures.insert(idx, SymbolKind::Function);
            }
            "definition.method" => {
                def_captures.insert(idx, SymbolKind::Method);
            }
            "definition.class" => {
                def_captures.insert(idx, SymbolKind::Class);
            }
            "definition.interface" => {
                def_captures.insert(idx, SymbolKind::Interface);
            }
            "definition.trait" => {
                def_captures.insert(idx, SymbolKind::Trait);
            }
            "definition.struct" => {
                def_captures.insert(idx, SymbolKind::Struct);
            }
            "definition.enum" => {
                def_captures.insert(idx, SymbolKind::Enum);
            }
            "definition.impl" => {
                def_captures.insert(idx, SymbolKind::Impl);
            }
            "definition.module" => {
                def_captures.insert(idx, SymbolKind::Module);
            }
            "definition.macro" => {
                def_captures.insert(idx, SymbolKind::Macro);
            }
            "definition.constant" => {
                def_captures.insert(idx, SymbolKind::Constant);
            }
            "definition.type" => {
                def_captures.insert(idx, SymbolKind::Type);
            }
            "reference.call" => {
                ref_captures.insert(idx, RefKind::Call);
            }
            "reference.implementation" => {
                ref_captures.insert(idx, RefKind::Implementation);
            }
            "reference.class" => {
                ref_captures.insert(idx, RefKind::Call);
            }
            "reference.type" => {} // skip type refs for now
            _ => {}
        }
    }

    let mut cursor = QueryCursor::new();
    let root = tree.root_node();
    let mut matches = cursor.matches(&query, root, code);

    let mut symbols = Vec::new();
    let mut refs = Vec::new();

    while let Some(m) = matches.next() {
        // Find the @name capture
        let name_node: Option<tree_sitter::Node> = name_idx.and_then(|idx| {
            m.captures
                .iter()
                .find(|c| c.index == idx as u32)
                .map(|c| c.node)
        });

        let name = match name_node {
            Some(n) => {
                let text = std::str::from_utf8(&code[n.byte_range()]).unwrap_or("");
                if text.is_empty() || text.len() > 200 {
                    continue;
                }
                text.to_string()
            }
            None => continue,
        };

        // Find the @doc capture if present
        let doc = doc_idx.and_then(|idx| {
            m.captures.iter().find(|c| c.index == idx as u32).map(|c| {
                let text = std::str::from_utf8(&code[c.node.byte_range()]).unwrap_or("");
                text.to_string()
            })
        });

        // Check if any capture is a definition or reference
        for capture in m.captures {
            if let Some(kind) = def_captures.get(&capture.index) {
                let node = capture.node;
                symbols.push(TaggedSymbol {
                    name: name.clone(),
                    kind: *kind,
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    start_col: node.start_position().column,
                    start_byte: node.start_byte(),
                    end_byte: node.end_byte(),
                    doc: doc.clone(),
                });
                break;
            }
            if let Some(kind) = ref_captures.get(&capture.index) {
                refs.push(TaggedRef {
                    name: name.clone(),
                    kind: *kind,
                    start_line: capture.node.start_position().row + 1,
                });
                break;
            }
        }
    }

    (symbols, refs)
}

/// Get the tags.scm query for a language.
pub fn tags_query(lang: &str) -> Option<&'static str> {
    match lang {
        "rust" => Some(TAGS_RUST),
        "python" => Some(TAGS_PYTHON),
        "javascript" | "jsx" => Some(TAGS_JAVASCRIPT),
        "typescript" | "tsx" => Some(TAGS_TYPESCRIPT),
        "go" => Some(TAGS_GO),
        "cpp" | "c" => Some(TAGS_CPP),
        "bash" => Some(TAGS_BASH),
        _ => None,
    }
}

/// Strip query predicates that tree-sitter 0.25 doesn't support.
/// Removes lines like (#strip! ...), (#select-adjacent! ...), (#set-adjacent! ...).
fn strip_unsupported_predicates(query: &str) -> String {
    query
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.starts_with("(#strip!")
                && !trimmed.starts_with("(#select-adjacent!")
                && !trimmed.starts_with("(#set-adjacent!")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ─── Embedded tags.scm queries ─────────────────────────────────────

const TAGS_RUST: &str = r#"
(struct_item
    name: (type_identifier) @name) @definition.struct

(enum_item
    name: (type_identifier) @name) @definition.enum

(union_item
    name: (type_identifier) @name) @definition.class

(type_item
    name: (type_identifier) @name) @definition.type

(declaration_list
    (function_item
        name: (identifier) @name) @definition.method)

(function_item
    name: (identifier) @name) @definition.function

(trait_item
    name: (type_identifier) @name) @definition.trait

(mod_item
    name: (identifier) @name) @definition.module

(macro_definition
    name: (identifier) @name) @definition.macro

(impl_item
    type: (type_identifier) @name) @definition.impl

(call_expression
    function: (identifier) @name) @reference.call

(call_expression
    function: (field_expression
        field: (field_identifier) @name)) @reference.call

(macro_invocation
    macro: (identifier) @name) @reference.call

(impl_item
    trait: (type_identifier) @name) @reference.implementation
"#;

const TAGS_PYTHON: &str = r#"
(class_definition
  name: (identifier) @name) @definition.class

(function_definition
  name: (identifier) @name) @definition.function

(call
  function: [
      (identifier) @name
      (attribute
        attribute: (identifier) @name)
  ]) @reference.call
"#;

const TAGS_JAVASCRIPT: &str = r#"
(method_definition
    name: (property_identifier) @name) @definition.method

[
    (class
      name: (_) @name)
    (class_declaration
      name: (_) @name)
] @definition.class

[
    (function_expression
      name: (identifier) @name)
    (function_declaration
      name: (identifier) @name)
    (generator_function
      name: (identifier) @name)
    (generator_function_declaration
      name: (identifier) @name)
] @definition.function

(lexical_declaration
    (variable_declarator
      name: (identifier) @name
      value: [(arrow_function) (function_expression)]) @definition.function)

(variable_declaration
    (variable_declarator
      name: (identifier) @name
      value: [(arrow_function) (function_expression)]) @definition.function)

(assignment_expression
  left: [
    (identifier) @name
    (member_expression
      property: (property_identifier) @name)
  ]
  right: [(arrow_function) (function_expression)]
) @definition.function

(pair
  key: (property_identifier) @name
  value: [(arrow_function) (function_expression)]) @definition.function

(call_expression
    function: (identifier) @name) @reference.call

(call_expression
  function: (member_expression
    property: (property_identifier) @name)) @reference.call

(new_expression
  constructor: (_) @name) @reference.class
"#;

// TypeScript: JavaScript queries plus interface and type-alias declarations
// (which only exist in the TS grammar).
const TAGS_TYPESCRIPT: &str = r#"
(method_definition
    name: (property_identifier) @name) @definition.method

[
    (class
      name: (_) @name)
    (class_declaration
      name: (_) @name)
] @definition.class

[
    (function_expression
      name: (identifier) @name)
    (function_declaration
      name: (identifier) @name)
    (generator_function
      name: (identifier) @name)
    (generator_function_declaration
      name: (identifier) @name)
] @definition.function

(lexical_declaration
    (variable_declarator
      name: (identifier) @name
      value: [(arrow_function) (function_expression)]) @definition.function)

(variable_declaration
    (variable_declarator
      name: (identifier) @name
      value: [(arrow_function) (function_expression)]) @definition.function)

(assignment_expression
  left: [
    (identifier) @name
    (member_expression
      property: (property_identifier) @name)
  ]
  right: [(arrow_function) (function_expression)]
) @definition.function

(pair
  key: (property_identifier) @name
  value: [(arrow_function) (function_expression)]) @definition.function

(call_expression
    function: (identifier) @name) @reference.call

(call_expression
  function: (member_expression
    property: (property_identifier) @name)) @reference.call

(new_expression
  constructor: (_) @name) @reference.class

(interface_declaration
    name: (type_identifier) @name) @definition.interface

(type_alias_declaration
    name: (type_identifier) @name) @definition.type
"#;

const TAGS_GO: &str = r#"
(function_declaration
    name: (identifier) @name) @definition.function

(method_declaration
    name: (field_identifier) @name) @definition.method

(call_expression
  function: [
    (identifier) @name
    (parenthesized_expression (identifier) @name)
    (selector_expression field: (field_identifier) @name)
    (parenthesized_expression (selector_expression field: (field_identifier) @name))
  ]) @reference.call

(type_spec
  name: (type_identifier) @name) @definition.type
"#;

const TAGS_CPP: &str = r#"
(struct_specifier name: (type_identifier) @name body:(_)) @definition.class
(class_specifier name: (type_identifier) @name) @definition.class
(declaration type: (union_specifier name: (type_identifier) @name)) @definition.class

; Function/method definitions with bodies. Anchoring on `function_definition`
; (rather than the bare `function_declarator`) makes the captured byte range
; cover the body, which the call-edge walker needs to recurse into.
(function_definition
    declarator: (function_declarator
        declarator: (identifier) @name)) @definition.function

(function_definition
    declarator: (function_declarator
        declarator: (field_identifier) @name)) @definition.method

(function_definition
    declarator: (function_declarator
        declarator: (qualified_identifier
            name: (identifier) @name))) @definition.method

(function_definition
    declarator: (function_declarator
        declarator: (destructor_name (identifier) @name))) @definition.method

; Header / out-of-line declarations (no body). These match free functions in
; a header (`int foo(int);`) and member functions inside a class body
; (`void bar();`). They never match inside a `function_definition`, so they
; complement the patterns above.
(declaration
    declarator: (function_declarator
        declarator: (identifier) @name)) @definition.function

(field_declaration
    declarator: (function_declarator
        declarator: (field_identifier) @name)) @definition.method

(type_definition declarator: (type_identifier) @name) @definition.type
(enum_specifier name: (type_identifier) @name) @definition.type

; Call references — function calls, method calls, qualified calls, template
; calls. Each becomes a `calls` edge resolved against in-file symbols.
(call_expression
    function: (identifier) @name) @reference.call

(call_expression
    function: (field_expression
        field: (field_identifier) @name)) @reference.call

(call_expression
    function: (qualified_identifier
        name: (identifier) @name)) @reference.call

(call_expression
    function: (template_function
        name: (identifier) @name)) @reference.call
"#;

const TAGS_BASH: &str = r#"
(function_definition
    name: (word) @name) @definition.function

(command
    name: (command_name) @name) @reference.call
"#;

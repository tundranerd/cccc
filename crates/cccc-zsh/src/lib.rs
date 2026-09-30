//! Zsh adapter: parses `zsh` source with the
//! [georgeharker/tree-sitter-zsh](https://github.com/georgeharker/tree-sitter-zsh)
//! grammar and lowers the concrete syntax tree into the language-agnostic
//! [`cccc_core::ir`].
//!
//! This is a pure library — it depends only on `cccc-core`, `cccc-shell-kit`,
//! `tree-sitter`, and the zsh grammar (whose C source is compiled by `cc`, so
//! like `cccc-py` there is no `libclang`/bindgen requirement), with no CLI
//! machinery. The unified `cccc` binary (the `cccc-cli` crate) registers this
//! adapter's [`analyze_source`]/[`DEFAULT_EXTS`] and dispatches `.zsh` files to
//! it.
//!
//! This crate contains **no scoring logic and no lowering logic** — the shared
//! lowering lives in [`cccc_shell_kit`] (also used by `cccc-sh`), and all
//! complexity rules live in [`cccc_core::engine`]. The zsh grammar is a
//! reworking that starts from the bash grammar, so its control-flow skeleton is
//! nearly identical; the shared builder covers it and adds zsh-only arms
//! (`repeat`, terse `for x (…)`, standalone `select`, `coproc`,
//! `{ … } always { … }`). This crate is just the grammar wiring.
//!
//! ## Known limitations
//!
//! - The grammar has **no rule for the braceless short form** `if [[ … ]] { … }`
//!   / `while … { … }`; those surface as `ERROR`/`MISSING` nodes and the parse
//!   error is reported in the summary. The `then … fi` form is required.
//! - `tree-sitter-zsh` is a fast-moving, less battle-tested grammar than
//!   `tree-sitter-bash`; expect a higher parse-error rate on advanced zsh
//!   (parameter-expansion flags, glob qualifiers, …). Errors are surfaced, never
//!   fatal.
//! - Discovery is extension-based, with shebang detection for extensionless
//!   files (handled by the CLI, not here).

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::Node;
use cccc_core::report::FileReport;
use cccc_shell_kit::Language;

/// File extensions analyzed by default (when `--ext` is not given).
pub const DEFAULT_EXTS: &[&str] = &["zsh"];

/// Parse `source` and produce its [`FileReport`], scoring via the core engine.
/// This is the convenience entry point used by the CLI; for the raw IR (e.g. to
/// feed a different consumer) use [`to_ir`].
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    let (nodes, parse_errors) = to_ir(path, source);
    engine::analyze(&path.display().to_string(), &nodes, parse_errors)
}

/// Parse `source` with `tree-sitter-zsh` and lower it to the complexity IR,
/// returning the module-level nodes plus any syntax-error messages. tree-sitter
/// always yields a tree (it recovers from errors by inserting `ERROR`/`MISSING`
/// nodes), so we still lower what parsed and report the error locations
/// alongside.
pub fn to_ir(_path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    cccc_shell_kit::to_ir(tree_sitter_zsh::LANGUAGE.into(), Language::Zsh, source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_core::report::FunctionReport;

    fn analyze(src: &str) -> FileReport {
        analyze_source(Path::new("test.zsh"), src)
    }

    fn find<'a>(fns: &'a [FunctionReport], name: &str) -> Option<&'a FunctionReport> {
        for f in fns {
            if f.name == name {
                return Some(f);
            }
            if let Some(found) = find(&f.children, name) {
                return Some(found);
            }
        }
        None
    }

    fn cognitive_of(src: &str, name: &str) -> u32 {
        find(&analyze(src).functions, name)
            .unwrap_or_else(|| panic!("function {name} not found"))
            .cognitive
    }

    fn cyclomatic_of(src: &str, name: &str) -> u32 {
        find(&analyze(src).functions, name)
            .unwrap_or_else(|| panic!("function {name} not found"))
            .cyclomatic
    }

    fn parse_errors(src: &str) -> Vec<String> {
        to_ir(Path::new("t.zsh"), src).1
    }

    #[test]
    fn sonar_sum_of_primes_is_7() {
        // The shared cross-language anchor. zsh has no labelled `continue`, so
        // the flat `else` supplies the 7th cognitive point the SonarSource
        // original gets from a labelled jump.
        let src = r#"
sum_of_primes() {
  local max=$1
  local total=0
  for ((i = 2; i <= max; i++)); do
    for ((j = 2; j < i; j++)); do
      if [ $((i % j)) -eq 0 ]; then
        total=$total
      else
        total=$((total + i))
      fi
    done
  done
  echo "$total"
}
"#;
        // for(+1) + nested for(+2) + nested if(+3) + else(+1 flat) = 7
        assert_eq!(cognitive_of(src, "sum_of_primes"), 7);
        // base 1 + for + for + if = 4
        assert_eq!(cyclomatic_of(src, "sum_of_primes"), 4);
    }

    #[test]
    fn sonar_get_words_is_1() {
        let src = r#"
get_words() {
  case "$1" in
    1) echo "one" ;;
    2) echo "a couple" ;;
    *) echo "lots" ;;
  esac
}
"#;
        // case(+1) = 1
        assert_eq!(cognitive_of(src, "get_words"), 1);
        // base 1 + 2 non-default arms = 3
        assert_eq!(cyclomatic_of(src, "get_words"), 3);
    }

    #[test]
    fn repeat_statement_is_a_loop() {
        let src = r#"
f() {
  repeat 3 do
    echo hi
  done
}
"#;
        // repeat(+1) = 1
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + repeat = 2
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn terse_for_is_a_loop() {
        let src = r#"
f() {
  for x (a b c) echo $x
}
"#;
        // terse for(+1) = 1
        assert_eq!(cognitive_of(src, "f"), 1);
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn select_is_a_loop() {
        let src = r#"
f() {
  select o in a b; do
    echo "$o"
  done
}
"#;
        // select(+1) = 1
        assert_eq!(cognitive_of(src, "f"), 1);
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn always_block_is_transparent() {
        let src = r#"
f() {
  {
    echo a
  } always {
    echo b
  }
}
"#;
        // `always` is a `finally`: no decision, so the function scores nothing.
        assert_eq!(cognitive_of(src, "f"), 0);
        // base 1 only
        assert_eq!(cyclomatic_of(src, "f"), 1);
    }

    #[test]
    fn coprocess_is_transparent() {
        let src = r#"
f() {
  coproc cat
}
"#;
        assert_eq!(cognitive_of(src, "f"), 0);
        assert_eq!(cyclomatic_of(src, "f"), 1);
    }

    #[test]
    fn case_fallthrough_does_not_add_a_decision() {
        let src = r#"
f() {
  case "$1" in
    a) echo a ;|
    b) echo b ;;
    *) echo c ;;
  esac
}
"#;
        // case(+1) = 1
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + 2 non-default arms (the `*)` arm is default) = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn logical_sequences_fold() {
        let src = r#"
f() {
  if [[ -n "$a" && "$b" == x || -z "$c" ]]; then
    :
  fi
}
"#;
        // if(+1) + && run(+1) + || run(+1) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + if 1 + (&& 2 operands => +1) + (|| 2 operands => +1) = 4
        assert_eq!(cyclomatic_of(src, "f"), 4);
    }

    #[test]
    fn recursion_adds_one_per_call() {
        let src = r#"
fib() {
  if [ "$1" -lt 2 ]; then
    echo "$1"
    return
  fi
  echo $(( $(fib $(($1 - 1))) + $(fib $(($1 - 2))) ))
}
"#;
        // if(+1) + two recursive calls(+2) = 3
        assert_eq!(cognitive_of(src, "fib"), 3);
    }

    #[test]
    fn nested_if_adds_nesting() {
        let src = r#"
f() {
  if [ "$1" ]; then
    if [ "$2" ]; then
      if [ "$3" ]; then
        :
      fi
    fi
  fi
}
"#;
        assert_eq!(cognitive_of(src, "f"), 6); // +1 +2 +3
    }

    #[test]
    fn braceless_if_is_reported_as_a_parse_error() {
        // zsh allows `if [[ … ]] { … }`, but the grammar has no rule for it,
        // so it surfaces as an error (documented limitation).
        let errors = parse_errors("if [[ -n $x ]] { echo yes }\n");
        assert!(!errors.is_empty());
    }
}

//! Shell adapter: parses POSIX `sh` / `bash` source with the first-party
//! [tree-sitter-bash](https://github.com/tree-sitter/tree-sitter-bash) grammar
//! and lowers the concrete syntax tree into the language-agnostic
//! [`cccc_core::ir`].
//!
//! This is a pure library — it depends only on `cccc-core`, `cccc-shell-kit`,
//! `tree-sitter`, and the Bash grammar (whose C source is compiled by `cc`, so
//! like `cccc-py` there is no `libclang`/bindgen requirement), with no CLI
//! machinery. The unified `cccc` binary (the `cccc-cli` crate) registers this
//! adapter's [`analyze_source`]/[`DEFAULT_EXTS`] and dispatches `.sh`/`.bash`/
//! `.bats`/`.ksh`/`.command` files to it.
//!
//! This crate contains **no scoring logic and no lowering logic** — the shared
//! lowering lives in [`cccc_shell_kit`] (also used by `cccc-zsh`), and all
//! complexity rules live in [`cccc_core::engine`]. This crate is just the
//! grammar wiring: it loads `tree-sitter-bash` and hands the CST to the shared
//! builder tagged [`Language::Sh`].
//!
//! See [`cccc_shell_kit`] for the full shell-to-IR mapping notes.

use std::path::Path;

use cccc_core::engine;
use cccc_core::ir::Node;
use cccc_core::report::FileReport;
use cccc_shell_kit::Language;

/// File extensions analyzed by default (when `--ext` is not given): POSIX shell
/// and its common dialects. `.bats` is the Bats test harness (bash), `.ksh` is
/// Korn shell, `.command` is macOS's double-clickable shell script.
pub const DEFAULT_EXTS: &[&str] = &["sh", "bash", "bats", "ksh", "command"];

/// Parse `source` and produce its [`FileReport`], scoring via the core engine.
/// This is the convenience entry point used by the CLI; for the raw IR (e.g. to
/// feed a different consumer) use [`to_ir`].
pub fn analyze_source(path: &Path, source: &str) -> FileReport {
    let (nodes, parse_errors) = to_ir(path, source);
    engine::analyze(&path.display().to_string(), &nodes, parse_errors)
}

/// Parse `source` with `tree-sitter-bash` and lower it to the complexity IR,
/// returning the module-level nodes plus any syntax-error messages. tree-sitter
/// always yields a tree (it recovers from errors by inserting `ERROR`/`MISSING`
/// nodes), so we still lower what parsed and report the error locations
/// alongside.
pub fn to_ir(_path: &Path, source: &str) -> (Vec<Node>, Vec<String>) {
    cccc_shell_kit::to_ir(tree_sitter_bash::LANGUAGE.into(), Language::Sh, source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_core::report::FunctionReport;

    fn analyze(src: &str) -> FileReport {
        analyze_source(Path::new("test.sh"), src)
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
        to_ir(Path::new("t.sh"), src).1
    }

    #[test]
    fn sonar_sum_of_primes_is_7() {
        // Shell has no labelled `continue`, so (as in the Python/Ruby fixtures)
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
        // if(+1) + nested if(+2) + nested if(+3) = 6
        assert_eq!(cognitive_of(src, "f"), 6);
        // base 1 + three ifs = 4
        assert_eq!(cyclomatic_of(src, "f"), 4);
    }

    #[test]
    fn elif_else_are_flat() {
        let src = r#"
f() {
  if [ "$1" ]; then
    :
  elif [ "$2" ]; then
    :
  else
    :
  fi
}
"#;
        // if(+1) + elif(+1 flat) + else(+1 flat) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + if + elif = 3 (else is not a decision point)
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn nested_construct_inside_elif_gets_the_elif_nesting() {
        let src = r#"
f() {
  if [ "$1" ]; then
    :
  elif [ "$2" ]; then
    if [ "$3" ]; then
      :
    fi
  fi
}
"#;
        // if(+1) + elif(+1 flat) + if nested in elif(+2) = 4
        assert_eq!(cognitive_of(src, "f"), 4);
    }

    #[test]
    fn multi_statement_condition_and_body() {
        let src = r#"
f() {
  if a; b; then
    c
    d
  fi
}
"#;
        // a single if decision, regardless of how many statements make up the
        // condition or the body
        assert_eq!(cognitive_of(src, "f"), 1);
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn loops_all_count() {
        let src = r#"
f() {
  while true; do
    :
  done
  until false; do
    :
  done
  for i in a b c; do
    :
  done
  select o in a b; do
    :
  done
  for ((i = 0; i < 3; i++)); do
    :
  done
}
"#;
        // five loops, each +1 at nesting 0
        assert_eq!(cognitive_of(src, "f"), 5);
        // base 1 + five loops = 6
        assert_eq!(cyclomatic_of(src, "f"), 6);
    }

    #[test]
    fn c_style_for_condition_does_not_add_a_decision() {
        let src = r#"
f() {
  for ((i = 0; i < 10; i++)); do
    :
  done
}
"#;
        // one loop; the `i < 10` comparison is not a decision
        assert_eq!(cognitive_of(src, "f"), 1);
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn case_default_is_not_a_decision() {
        let src = r#"
f() {
  case "$1" in
    a | b) :
      ;;
    c) :
      ;;
  esac
}
"#;
        // case(+1); no default arm -> both arms are decisions
        assert_eq!(cognitive_of(src, "f"), 1);
        // base 1 + 2 non-default arms = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn logical_sequences_fold_by_operator() {
        let src = r#"
f() {
  if [ "$a" ] && [ "$b" ] && [ "$c" ] || [ "$d" ]; then
    :
  fi
}
"#;
        // if(+1) + && run(+1) + || run(+1) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
        // base 1 + if 1 + (&& 3 operands => +2) + (|| 2 operands => +1) = 5
        assert_eq!(cyclomatic_of(src, "f"), 5);
    }

    #[test]
    fn test_command_brackets_fold_logical_operators() {
        let src = r#"
f() {
  if [[ -n "$a" && "$a" -gt 3 ]]; then
    :
  fi
}
"#;
        // if(+1) + && run(+1) = 2
        assert_eq!(cognitive_of(src, "f"), 2);
        // base 1 + if 1 + (&& 2 operands => +1) = 3
        assert_eq!(cyclomatic_of(src, "f"), 3);
    }

    #[test]
    fn posix_test_dash_a_folds() {
        let src = r#"
f() {
  if [ "$a" -a "$b" -o "$c" ]; then
    :
  fi
}
"#;
        // if(+1) + -a run(+1) + -o run(+1) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
    }

    #[test]
    fn arithmetic_ternary_is_a_conditional() {
        let src = r#"
f() {
  local x=$(( 1 ? 2 : 3 ))
}
"#;
        assert_eq!(cognitive_of(src, "f"), 1);
        assert_eq!(cyclomatic_of(src, "f"), 2);
    }

    #[test]
    fn negated_command_scores_nothing() {
        let src = r#"
f() {
  ! grep -q x file
}
"#;
        assert_eq!(cognitive_of(src, "f"), 0);
    }

    #[test]
    fn break_multilevel_is_a_labelled_jump() {
        let src = r#"
f() {
  while true; do
    while true; do
      break 2
    done
  done
}
"#;
        // while(+1) + nested while(+2) + `break 2`(+1 flat) = 4
        assert_eq!(cognitive_of(src, "f"), 4);
    }

    #[test]
    fn plain_break_and_continue_score_nothing() {
        let src = r#"
f() {
  for i in a b; do
    if [ "$i" ]; then
      continue
    fi
    break
  done
}
"#;
        // for(+1) + nested if(+2) = 3; plain break/continue add nothing
        assert_eq!(cognitive_of(src, "f"), 3);
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
    fn nested_function_is_its_own_unit() {
        let src = r#"
host() {
  inner() {
    if [ "$1" ]; then
      :
    fi
  }
}
"#;
        assert_eq!(cognitive_of(src, "host"), 0);
        assert_eq!(cognitive_of(src, "inner"), 1);
    }

    #[test]
    fn function_keyword_forms_are_found() {
        let src = r#"
function a {
  if true; then :; fi
}
function b() {
  if true; then :; fi
}
c() {
  if true; then :; fi
}
"#;
        assert_eq!(cognitive_of(src, "a"), 1);
        assert_eq!(cognitive_of(src, "b"), 1);
        assert_eq!(cognitive_of(src, "c"), 1);
        // all three are "function" units
        assert_eq!(find(&analyze(src).functions, "a").unwrap().kind, "function");
    }

    #[test]
    fn module_level_code_counts_toward_the_file() {
        let src = r#"
if [ "$1" ]; then
  echo hi
fi
"#;
        let report = analyze(src);
        assert_eq!(report.cognitive, 1);
        assert!(report.functions.is_empty());
    }

    #[test]
    fn constructs_in_subshell_and_command_substitution_count() {
        let src = r#"
f() {
  local x=$(if [ "$1" ]; then echo a; else echo b; fi)
  ( for i in a b; do echo "$i"; done )
}
"#;
        // if in `$( )`(+1) + its else(+1 flat) + loop in `( )`(+1) = 3
        assert_eq!(cognitive_of(src, "f"), 3);
    }

    #[test]
    fn command_substitution_inside_heredoc_body_counts() {
        let src = r#"
f() {
  cat <<EOF
$(if [ "$1" ]; then echo a; fi)
EOF
}
"#;
        // the `if` inside the heredoc's `$( )` still counts (+1)
        assert_eq!(cognitive_of(src, "f"), 1);
    }

    #[test]
    fn comment_does_not_change_score() {
        let plain = "f() {\n  if [ \"$a\" ]; then\n    if [ \"$b\" ]; then :; fi\n  fi\n}\n";
        let commented =
            "f() {\n  # c\n  if [ \"$a\" ]; then # c\n    if [ \"$b\" ]; then :; fi\n  fi\n}\n";
        // if(+1) + nested if(+2) = 3, with or without comments
        assert_eq!(cognitive_of(plain, "f"), 3);
        assert_eq!(cognitive_of(commented, "f"), cognitive_of(plain, "f"));
    }

    #[test]
    fn file_total_sums_all_functions() {
        let src = r#"
a() {
  if [ "$1" ]; then :; fi
}
b() {
  if [ "$2" ]; then :; fi
}
"#;
        assert_eq!(analyze(src).cognitive, 2);
    }

    #[test]
    fn parse_error_is_reported() {
        // tree-sitter is fault-tolerant: it still yields a (partial) tree but
        // surfaces the error location for the broken input.
        let errors = parse_errors("f() {\n  if true; then\n    echo hi\n}\n");
        assert!(!errors.is_empty());
    }
}

//! Intra-line word-diff: tokenization, token-level LCS, changed-range
//! building, similarity gating, and remove/add line pairing.
//!
//! Ported from revdiff's `worddiff` package. The tokenizer mirrors its regex
//! `[\pL\pN_]+|\s+|[^\pL\pN_\s]+`: maximal runs of word chars (Unicode
//! letters/digits/underscore), whitespace, or punctuation. Offsets are byte
//! offsets into the original line.

/// A changed byte-offset range within a line. `end` is exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Range {
    pub start: u32,
    pub end: u32,
}

/// Intra-line diff is skipped for lines longer than this many bytes to avoid
/// O(m*n) LCS blowup on pathological input (minified files, long configs).
pub const MAX_LINE_LEN_FOR_DIFF: usize = 500;

/// Minimum percentage of common non-whitespace tokens for highlighting; pairs
/// below this get no intra-line overlay.
pub const SIMILARITY_THRESHOLD: u32 = 30;

/// Greedy pairing is skipped for change blocks where removes*adds exceeds
/// this product; such blocks fall back to positional 1:1 pairing. This is a
/// deliberate guard the Go original lacked, reachable only on pathological
/// blocks (thousands of unequal removes and adds).
const MAX_GREEDY_PAIR_PRODUCT: usize = 250_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenClass {
    Word,
    Whitespace,
    Punct,
}

/// Whitespace per the ported tokenizer: ASCII whitespace as matched by Go's
/// regexp `\s` class.
fn char_class(c: char) -> TokenClass {
    if c.is_alphanumeric() || c == '_' {
        TokenClass::Word
    } else if matches!(c, ' ' | '\t' | '\n' | '\x0c' | '\r') {
        TokenClass::Whitespace
    } else {
        TokenClass::Punct
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Token<'a> {
    text: &'a str,
    start: u32,
    end: u32,
    class: TokenClass,
}

fn is_whitespace_token(t: &Token) -> bool {
    t.class == TokenClass::Whitespace
}

/// Split a line into maximal runs of word, whitespace, and punctuation chars.
fn tokenize(line: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut run_start: usize = 0;
    let mut run_class: Option<TokenClass> = None;
    for (idx, c) in line.char_indices() {
        let class = char_class(c);
        match run_class {
            Some(current) if current == class => {}
            Some(current) => {
                tokens.push(Token {
                    text: &line[run_start..idx],
                    start: run_start as u32,
                    end: idx as u32,
                    class: current,
                });
                run_start = idx;
                run_class = Some(class);
            }
            None => {
                run_start = idx;
                run_class = Some(class);
            }
        }
    }
    if let Some(current) = run_class {
        tokens.push(Token {
            text: &line[run_start..],
            start: run_start as u32,
            end: line.len() as u32,
            class: current,
        });
    }
    tokens
}

/// Compute which tokens on each side are kept (unchanged) via token-level LCS.
/// Returns boolean vectors parallel to the token slices: true = kept.
fn lcs_kept_tokens(minus: &[Token], plus: &[Token]) -> (Vec<bool>, Vec<bool>) {
    let m = minus.len();
    let n = plus.len();
    if m == 0 || n == 0 {
        return (vec![false; m], vec![false; n]);
    }

    let mut dp = vec![0u32; (m + 1) * (n + 1)];
    let at = |i: usize, j: usize| i * (n + 1) + j;
    for i in 1..=m {
        for j in 1..=n {
            dp[at(i, j)] = if minus[i - 1].text == plus[j - 1].text {
                dp[at(i - 1, j - 1)] + 1
            } else if dp[at(i - 1, j)] >= dp[at(i, j - 1)] {
                dp[at(i - 1, j)]
            } else {
                dp[at(i, j - 1)]
            };
        }
    }

    let mut keep_minus = vec![false; m];
    let mut keep_plus = vec![false; n];
    let (mut i, mut j) = (m, n);
    while i > 0 && j > 0 {
        if minus[i - 1].text == plus[j - 1].text {
            keep_minus[i - 1] = true;
            keep_plus[j - 1] = true;
            i -= 1;
            j -= 1;
        } else if dp[at(i - 1, j)] >= dp[at(i, j - 1)] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    (keep_minus, keep_plus)
}

/// Convert token keep flags into merged byte-offset ranges. Adjacent changed
/// non-whitespace tokens merge into one range; whitespace tokens are excluded
/// and break merging.
fn build_changed_ranges(tokens: &[Token], keep: &[bool]) -> Vec<Range> {
    let mut ranges: Vec<Range> = Vec::new();
    let mut cur: Option<Range> = None;
    for (i, tok) in tokens.iter().enumerate() {
        if keep[i] || is_whitespace_token(tok) {
            if let Some(r) = cur.take() {
                ranges.push(r);
            }
            continue;
        }
        match cur.as_mut() {
            Some(r) => r.end = tok.end,
            None => {
                cur = Some(Range {
                    start: tok.start,
                    end: tok.end,
                })
            }
        }
    }
    if let Some(r) = cur {
        ranges.push(r);
    }
    ranges
}

fn count_non_whitespace(tokens: &[Token]) -> usize {
    tokens.iter().filter(|t| !is_whitespace_token(t)).count()
}

/// True when the pair shares at least [`SIMILARITY_THRESHOLD`] percent common
/// non-whitespace tokens, measured against the shorter side.
fn passes_similarity_gate(minus: &[Token], plus: &[Token], keep_minus: &[bool]) -> bool {
    let equal_non_ws = keep_minus
        .iter()
        .zip(minus)
        .filter(|(k, t)| **k && !is_whitespace_token(t))
        .count();
    let shorter = count_non_whitespace(minus).min(count_non_whitespace(plus));
    if shorter == 0 {
        return false;
    }
    equal_non_ws * 100 >= shorter * SIMILARITY_THRESHOLD as usize
}

/// Compute changed byte-offset ranges for a minus/plus line pair.
///
/// Returns None when either line is empty, exceeds
/// [`MAX_LINE_LEN_FOR_DIFF`], tokenizes to nothing, is identical after
/// tokenization, or fails the similarity gate.
pub fn compute_intra_ranges(minus_line: &str, plus_line: &str) -> Option<(Vec<Range>, Vec<Range>)> {
    if minus_line.is_empty() || plus_line.is_empty() {
        return None;
    }
    if minus_line.len() > MAX_LINE_LEN_FOR_DIFF || plus_line.len() > MAX_LINE_LEN_FOR_DIFF {
        return None;
    }

    let minus_toks = tokenize(minus_line);
    let plus_toks = tokenize(plus_line);
    if minus_toks.is_empty() || plus_toks.is_empty() {
        return None;
    }

    let (keep_minus, keep_plus) = lcs_kept_tokens(&minus_toks, &plus_toks);
    let minus_ranges = build_changed_ranges(&minus_toks, &keep_minus);
    let plus_ranges = build_changed_ranges(&plus_toks, &keep_plus);
    if minus_ranges.is_empty() && plus_ranges.is_empty() {
        return None; // identical lines after tokenization
    }

    if !passes_similarity_gate(&minus_toks, &plus_toks, &keep_minus) {
        return None;
    }

    Some((minus_ranges, plus_ranges))
}

/// A line of content with its change direction, input to [`pair_lines`].
#[derive(Debug, Clone, Copy)]
pub struct LinePair<'a> {
    pub content: &'a str,
    pub is_remove: bool,
}

/// A matched remove/add pair; indices point into the [`pair_lines`] input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pair {
    pub remove_idx: usize,
    pub add_idx: usize,
}

/// Pair remove and add lines within one contiguous change block.
/// Equal-length runs pair 1:1 in order; unequal runs use greedy best-match
/// scoring by common prefix plus suffix bytes. The greedy pass doubles as the
/// add-vs-modified classifier: unpaired lines are pure additions/removals.
pub fn pair_lines(lines: &[LinePair]) -> Vec<Pair> {
    let mut removes = Vec::new();
    let mut adds = Vec::new();
    for (i, lp) in lines.iter().enumerate() {
        if lp.is_remove {
            removes.push(i);
        } else {
            adds.push(i);
        }
    }

    if removes.is_empty() || adds.is_empty() {
        return Vec::new();
    }

    if removes.len() == adds.len() {
        return removes
            .iter()
            .zip(&adds)
            .map(|(&r, &a)| Pair {
                remove_idx: r,
                add_idx: a,
            })
            .collect();
    }

    if removes.len().saturating_mul(adds.len()) > MAX_GREEDY_PAIR_PRODUCT {
        // Pathological block: positional 1:1 pairing of the overlapping run.
        return removes
            .iter()
            .zip(&adds)
            .map(|(&r, &a)| Pair {
                remove_idx: r,
                add_idx: a,
            })
            .collect();
    }

    greedy_pair(lines, &removes, &adds)
}

/// Greedy best-match pairing: iterate the shorter side, pick the best unused
/// line from the longer side by 2*prefix + 2*suffix byte score.
fn greedy_pair(lines: &[LinePair], removes: &[usize], adds: &[usize]) -> Vec<Pair> {
    let (shorter, longer, shorter_is_remove) = if adds.len() < removes.len() {
        (adds, removes, false)
    } else {
        (removes, adds, true)
    };

    let mut used = vec![false; longer.len()];
    let mut pairs = Vec::with_capacity(shorter.len());

    for &si in shorter {
        let mut best_score: i64 = -1;
        let mut best_idx: Option<usize> = None;
        let s_content = lines[si].content;

        for (li, &li2) in longer.iter().enumerate() {
            if used[li] {
                continue;
            }
            let l_content = lines[li2].content;
            let score = 2 * common_prefix_len(s_content, l_content) as i64
                + 2 * common_suffix_len(s_content, l_content) as i64;
            if score > best_score {
                best_score = score;
                best_idx = Some(li);
            }
        }

        if let Some(bi) = best_idx {
            used[bi] = true;
            pairs.push(if shorter_is_remove {
                Pair {
                    remove_idx: si,
                    add_idx: longer[bi],
                }
            } else {
                Pair {
                    remove_idx: longer[bi],
                    add_idx: si,
                }
            });
        }
    }
    pairs
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let n = a.len().min(b.len());
    for i in 0..n {
        if a[i] != b[i] {
            return i;
        }
    }
    n
}

fn common_suffix_len(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let n = a.len().min(b.len());
    for i in 0..n {
        if a[a.len() - 1 - i] != b[b.len() - 1 - i] {
            return i;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranges(pairs: &[(u32, u32)]) -> Vec<Range> {
        pairs
            .iter()
            .map(|&(start, end)| Range { start, end })
            .collect()
    }

    #[test]
    fn identical_lines_yield_none() {
        assert_eq!(compute_intra_ranges("same line", "same line"), None);
    }

    #[test]
    fn empty_side_yields_none() {
        assert_eq!(compute_intra_ranges("", "x"), None);
        assert_eq!(compute_intra_ranges("x", ""), None);
    }

    #[test]
    fn single_word_change_is_ranged() {
        let (minus, plus) =
            compute_intra_ranges("let x = foo(1)", "let x = bar(1)").expect("ranges");
        assert_eq!(minus, ranges(&[(8, 11)]));
        assert_eq!(plus, ranges(&[(8, 11)]));
    }

    #[test]
    fn disjoint_lines_fail_similarity_gate() {
        assert_eq!(
            compute_intra_ranges("alpha beta gamma", "delta epsilon zeta"),
            None
        );
    }

    #[test]
    fn whitespace_only_difference_yields_none() {
        // Tokens differ only in whitespace runs; changed ranges exclude
        // whitespace so both sides produce no ranges.
        assert_eq!(compute_intra_ranges("a  b", "a b"), None);
    }

    #[test]
    fn adjacent_changed_tokens_merge_into_one_range() {
        // "foo(x)" -> "bar[y]": word+punct+word+punct all changed, merged.
        let (minus, plus) =
            compute_intra_ranges("keep foo(x) keep", "keep bar[y] keep").expect("ranges");
        assert_eq!(minus, ranges(&[(5, 11)]));
        assert_eq!(plus, ranges(&[(5, 11)]));
    }

    #[test]
    fn whitespace_breaks_range_merging() {
        let (minus, _plus) =
            compute_intra_ranges("keep aa bb keep", "keep xx yy keep").expect("ranges");
        assert_eq!(minus, ranges(&[(5, 7), (8, 10)]));
    }

    #[test]
    fn punctuation_runs_are_single_tokens() {
        let toks = tokenize("a ==> b");
        let texts: Vec<&str> = toks.iter().map(|t| t.text).collect();
        assert_eq!(texts, vec!["a", " ", "==>", " ", "b"]);
    }

    #[test]
    fn cjk_and_emoji_are_word_chars_with_byte_offsets() {
        let toks = tokenize("\u{4f60}\u{597d} x");
        assert_eq!(toks[0].text, "\u{4f60}\u{597d}");
        assert_eq!((toks[0].start, toks[0].end), (0, 6)); // 2 CJK chars = 6 bytes
        // Emoji are neither alphanumeric nor whitespace: punctuation-class run.
        let toks = tokenize("\u{1F600}\u{1F601}");
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].class, TokenClass::Punct);
        assert_eq!((toks[0].start, toks[0].end), (0, 8));
    }

    #[test]
    fn similarity_gate_on_point() {
        // shorter side 10 non-ws tokens (5 words + 4 spaces? no: spaces are ws).
        // Build: minus has words a b c d e f g h i j (10 non-ws tokens),
        // plus keeps exactly 3 of them: 3/10 = 30% >= threshold -> passes.
        let minus = "a b c d e f g h i j";
        let plus = "a b c X Y Z W V U T";
        let got = compute_intra_ranges(minus, plus);
        assert!(got.is_some(), "30% exactly must pass the gate");
    }

    #[test]
    fn similarity_gate_off_point() {
        // 2 of 10 kept = 20% < 30% -> gated off.
        let minus = "a b c d e f g h i j";
        let plus = "a b X Y Z W V U T S";
        assert_eq!(compute_intra_ranges(minus, plus), None);
    }

    #[test]
    fn line_length_cap_on_point_allows_diff() {
        // Exactly 500 bytes passes; the shared prefix keeps similarity high.
        let base = "k ".repeat(248); // 496 bytes of alternating word/space
        let minus = format!("{base}aaaa"); // 500 bytes
        let plus = format!("{base}bbbb");
        assert_eq!(minus.len(), MAX_LINE_LEN_FOR_DIFF);
        assert!(compute_intra_ranges(&minus, &plus).is_some());
    }

    #[test]
    fn line_length_cap_off_point_skips_diff() {
        let base = "k ".repeat(248);
        let minus = format!("{base}aaaaa"); // 501 bytes
        let plus = format!("{base}bbbb");
        assert_eq!(minus.len(), MAX_LINE_LEN_FOR_DIFF + 1);
        assert_eq!(compute_intra_ranges(&minus, &plus), None);
    }

    #[test]
    fn equal_runs_pair_one_to_one_in_order() {
        let lines = [
            LinePair {
                content: "r1",
                is_remove: true,
            },
            LinePair {
                content: "r2",
                is_remove: true,
            },
            LinePair {
                content: "a1",
                is_remove: false,
            },
            LinePair {
                content: "a2",
                is_remove: false,
            },
        ];
        let pairs = pair_lines(&lines);
        assert_eq!(
            pairs,
            vec![
                Pair {
                    remove_idx: 0,
                    add_idx: 2
                },
                Pair {
                    remove_idx: 1,
                    add_idx: 3
                },
            ]
        );
    }

    #[test]
    fn unequal_runs_pair_greedily_by_prefix_suffix_score() {
        // One remove, two adds: the remove should pair with the similar add,
        // not positionally with the first.
        let lines = [
            LinePair {
                content: "let value = compute();",
                is_remove: true,
            },
            LinePair {
                content: "// brand new comment",
                is_remove: false,
            },
            LinePair {
                content: "let value = compute_v2();",
                is_remove: false,
            },
        ];
        let pairs = pair_lines(&lines);
        assert_eq!(
            pairs,
            vec![Pair {
                remove_idx: 0,
                add_idx: 2
            }]
        );
    }

    #[test]
    fn pairing_score_tie_takes_first_candidate() {
        // Both adds score identically against the remove; the first unused
        // candidate wins (strictly-greater comparison keeps the first).
        let lines = [
            LinePair {
                content: "xxx",
                is_remove: true,
            },
            LinePair {
                content: "yyy",
                is_remove: false,
            },
            LinePair {
                content: "zzz",
                is_remove: false,
            },
        ];
        let pairs = pair_lines(&lines);
        assert_eq!(
            pairs,
            vec![Pair {
                remove_idx: 0,
                add_idx: 1
            }]
        );
    }

    #[test]
    fn no_removes_or_no_adds_pairs_nothing() {
        let only_adds = [
            LinePair {
                content: "a",
                is_remove: false,
            },
            LinePair {
                content: "b",
                is_remove: false,
            },
        ];
        assert!(pair_lines(&only_adds).is_empty());
        let only_removes = [LinePair {
            content: "a",
            is_remove: true,
        }];
        assert!(pair_lines(&only_removes).is_empty());
    }

    #[test]
    fn ranges_are_always_in_bounds_and_ordered() {
        let minus = "fn process(input: &str) -> Result<Output, Error>";
        let plus = "fn process(input: &[u8]) -> Result<Output, ProcessError>";
        let (mr, pr) = compute_intra_ranges(minus, plus).expect("ranges");
        for r in mr {
            assert!(r.start < r.end);
            assert!((r.end as usize) <= minus.len());
            assert!(minus.is_char_boundary(r.start as usize));
            assert!(minus.is_char_boundary(r.end as usize));
        }
        for r in pr {
            assert!(r.start < r.end);
            assert!((r.end as usize) <= plus.len());
        }
    }
}

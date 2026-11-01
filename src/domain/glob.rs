/// Anchored glob over the whole text: `*` any run (incl. empty), `?` one char, rest literal.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    // Iterative two-pointer match with single-star backtracking: O(p*t) worst case, no recursion.
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            mark += 1;
            ti = mark;
            pi = s + 1;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn glob_anchored() {
        assert!(glob_match("0.1.*", "0.1.5-dev"));
        assert!(!glob_match("0.1.*", "10.1.0"));
        assert!(glob_match("0.1*", "0.10.0"));
        assert!(glob_match("?.0", "1.0"));
        assert!(!glob_match("?.0", "10.0"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a*b*c", "aXbYc"));
        assert!(!glob_match("a*b*c", "aXbY"));
        assert!(!glob_match("a.c", "axc"));
        assert!(glob_match("a.c", "a.c"));
        assert!(glob_match("*b", "abab"));
        assert!(!glob_match("a", "ab"));
        assert!(glob_match("é?", "éß"));
    }
}

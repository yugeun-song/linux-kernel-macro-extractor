use std::fmt::Write as _;
use std::process::Command;

use kmx::analysis::c_expr::simplify;
use kmx::analysis::c_type::Basic;
use kmx::intern::Interner;
use kmx::syntax::lex::lex_all;
use rustc_hash::FxHashSet;

const LP64: Basic = Basic { lp64: true };

const SIZED: &[&str] = &[
    "char",
    "short",
    "int",
    "long",
    "unsigned long long",
    "_Bool",
    "void *",
    "int [3]",
    "char *[4]",
    "long (*)[5]",
    "unsigned short [2][3]",
    "double",
    "float",
];

const BUILTINS: &[&str] = &[
    "__builtin_clz",
    "__builtin_clzl",
    "__builtin_clzll",
    "__builtin_ctz",
    "__builtin_ctzl",
    "__builtin_ctzll",
    "__builtin_popcount",
    "__builtin_popcountl",
    "__builtin_popcountll",
    "__builtin_parity",
    "__builtin_parityll",
    "__builtin_ffs",
    "__builtin_ffsl",
    "__builtin_ffsll",
    "__builtin_clrsb",
    "__builtin_clrsbll",
    "__builtin_bswap32",
    "__builtin_bswap64",
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

const LEAVES: &[&str] = &["a", "b", "c", "n"];
const LITERALS: &[&str] = &[
    "0",
    "1",
    "2",
    "7",
    "39",
    "0x10",
    "3UL",
    "1U",
    "0x80000000",
    "2147483647",
    "2147483648",
    "4294967295U",
    "1LL",
    "0xffffffffffffffffUL",
    "0777",
    "0b101",
    "100u",
    "5lu",
    "1ull",
    "9223372036854775807L",
    "12",
    "63",
];
const BINARY: &[&str] = &[
    "*", "/", "%", "+", "-", "<<", ">>", "<", ">", "<=", ">=", "==", "!=", "&", "^", "|", "&&",
    "||",
];
const CASTS: &[&str] = &[
    "int",
    "long",
    "unsigned long",
    "unsigned char",
    "T",
    "long long",
    "const int",
    "U",
    "unsigned",
    "unsigned int",
    "short",
    "_Bool",
    "signed char",
    "unsigned long long",
];

fn wrap(r: &mut Rng, s: String, needed: bool) -> String {
    if needed || r.chance(35) {
        if r.chance(15) {
            format!("(({s}))")
        } else {
            format!("({s})")
        }
    } else {
        s
    }
}

fn block(r: &mut Rng, depth: u32, id: &mut u32) -> String {
    *id += 1;
    let n = *id;
    let mut sub = |r: &mut Rng| expr(r, depth.saturating_sub(1), id).0;
    match r.below(11) {
        0 => {
            let (x, y) = (sub(r), sub(r));
            format!("({{ int t{n} = {x}; (t{n}) + {} ; }})", wrap(r, y, true))
        }
        1 => {
            let (x, y) = (sub(r), sub(r));
            format!("({{ T t{n} = (T)({x}); (t{n}) & {}; }})", wrap(r, y, true))
        }
        2 => {
            let (x, y, z) = (sub(r), sub(r), sub(r));
            format!(
                "({{ int t{n} = {x}; ((void)(sizeof(( long)({y})))); (t{n}) + {}; }})",
                wrap(r, z, true)
            )
        }
        3 => {
            let (x, z) = (sub(r), sub(r));
            format!(
                "({{ int t{n} = {x}; (void)0; ; (t{n}) - {}; }})",
                wrap(r, z, true)
            )
        }
        4 => {
            let (x, y) = (sub(r), sub(r));
            format!("({{ long u{n} = {x}, v{n} = ({y}); (u{n}) - (v{n}); }})")
        }
        5 => "({ int T = 3; (T) - 1; })".to_string(),
        6 => {
            let x = sub(r);
            format!(
                "({{ U *q{n} = (U *)&p[1]; *q{n} + {}; }})",
                wrap(r, x, true)
            )
        }
        7 => {
            let x = sub(r);
            format!("({{ (void)sizeof(int[(c) + 1]); {x}; }})")
        }
        8 => {
            let (x, y) = (sub(r), sub(r));
            format!("({{ ((void)(sizeof(( long)({y})))); int t{n} = {x}; t{n}; }})")
        }
        9 => {
            let (x, y) = (sub(r), sub(r));
            format!(
                "({{ int t{n} = {x}; (void)t{n}; (void)s->x; (void)f(1); (void)(1 + 2); (t{n}) + {}; }})",
                wrap(r, y, true)
            )
        }
        _ => {
            let (x, y) = (sub(r), sub(r));
            format!("({{ int a = {x}; (a) * {}; }})", wrap(r, y, true))
        }
    }
}

fn expr(r: &mut Rng, depth: u32, id: &mut u32) -> (String, u8) {
    if depth == 0 || r.chance(18) {
        let leaf = match r.below(18) {
            0 => format!("p[{}]", expr(r, depth.saturating_sub(1), id).0),
            1 => "s->x".to_string(),
            2 => "(*s).x".to_string(),
            3 => format!("f({})", expr(r, depth.saturating_sub(1), id).0),
            4 => "sizeof(T)".to_string(),
            5 => "*p".to_string(),
            6 => "((struct S *)s)->x".to_string(),
            7 => "*(int *)p".to_string(),
            8 => "(*(int (*)[64])&p)[1]".to_string(),
            9 => format!(
                "({{ int t = {}; t; }})",
                expr(r, depth.saturating_sub(1), id).0
            ),
            10 => "sizeof(\"ab\" \"c\")".to_string(),
            11 => "((int (*)(int))f)(2)".to_string(),
            12 | 13 => block(r, depth, id),
            14 | 15 => r.pick(LEAVES).to_string(),
            _ => r.pick(LITERALS).to_string(),
        };
        return (leaf, 16);
    }
    match r.below(12) {
        0..=5 => {
            let op = r.pick(BINARY);
            let (l, lp) = expr(r, depth - 1, id);
            let (rr, rp) = expr(r, depth - 1, id);
            let l = wrap(r, l, lp < 13);
            let rr = wrap(r, rr, rp < 14);
            (format!("{l} {op} {rr}"), 4)
        }
        6 => {
            let ty = r.pick(CASTS);
            let (e, p) = expr(r, depth - 1, id);
            let e = wrap(r, e, p < 14);
            (format!("({ty}){e}"), 14)
        }
        7 => {
            let op = r.pick(&["-", "+", "!", "~"]);
            let (e, p) = expr(r, depth - 1, id);
            let clash = e.starts_with(op);
            let e = wrap(r, e, p < 14 || clash);
            (format!("{op}{e}"), 14)
        }
        8 => {
            let (c, cp) = expr(r, depth - 1, id);
            let (t, _) = expr(r, depth - 1, id);
            let (f, fp) = expr(r, depth - 1, id);
            let c = wrap(r, c, cp < 5);
            let f = wrap(r, f, fp < 4);
            (format!("{c} ? {t} : {f}"), 3)
        }
        9 => {
            let (l, _) = expr(r, depth - 1, id);
            let (rr, rp) = expr(r, depth - 1, id);
            let rr = wrap(r, rr, rp < 2);
            (format!("({l}, {rr})"), 16)
        }
        10 => (block(r, depth, id), 16),
        _ => {
            let (e, _) = expr(r, depth - 1, id);
            (format!("sizeof({e})"), 14)
        }
    }
}

fn bodies(dump: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut name = String::new();
    let mut body = String::new();
    for line in dump.lines() {
        if let Some(rest) = line.strip_prefix(";; Function ") {
            if !name.is_empty() {
                out.push((name.clone(), body.clone()));
            }
            name = rest.split_whitespace().next().unwrap_or("").to_string();
            body.clear();
        } else if !line.starts_with(";;") {
            body.push_str(line);
            body.push('\n');
        }
    }
    if !name.is_empty() {
        out.push((name, body));
    }
    out
}

#[test]
fn simplified_expressions_compile_to_the_same_tree() {
    let Some(count) = std::env::var("KMX_C_EXPR_ORACLE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    else {
        eprintln!("KMX_C_EXPR_ORACLE is not set; skipping the C expression oracle");
        return;
    };
    let seed: u64 = std::env::var("KMX_C_EXPR_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x9e37_79b9_7f4a_7c15);
    let mut r = Rng(seed | 1);
    let dir = std::env::temp_dir().join(format!("kmx-c-expr-oracle-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cases = Vec::new();
    let mut changed = 0;
    let mut src = String::from(
        "typedef int T;\ntypedef unsigned U;\nstruct S { int x; };\nint f(int);\n\
         long n;\nint p[64];\n",
    );
    let params = "int a, int b, int c, struct S *s";
    for i in 0..count {
        let depth = 1 + r.below(6) as u32;
        let mut id = 0;
        let (e, _) = expr(&mut r, depth, &mut id);
        let mut it = Interner::new();
        let toks = lex_all(e.as_bytes(), false, &mut it);
        let known: FxHashSet<_> = if i % 2 == 0 {
            ["a", "b", "c"].iter().map(|k| it.intern(k)).collect()
        } else {
            FxHashSet::default()
        };
        let simple = simplify(&toks, &it, &known, &LP64);
        if let Some(s) = &simple
            && s != &e
        {
            changed += 1;
        }
        let simple = simple.unwrap_or_else(|| e.clone());
        let _ = writeln!(
            src,
            "long o{i}({params}) {{ return {e}; }}\n\
             long s{i}({params}) {{ return {simple}; }}\n\
             void t{i}({params}) {{ _Static_assert(__builtin_types_compatible_p(__typeof__({e}), __typeof__({simple})), \"t{i}\"); }}"
        );
        cases.push((e, simple));
    }
    let file = dir.join("cases.c");
    std::fs::write(&file, &src).unwrap();
    let out = Command::new("gcc")
        .args([
            "-c",
            "-w",
            "-std=gnu11",
            "-fdump-tree-original=-",
            "-o",
            "/dev/null",
        ])
        .arg(&file)
        .output()
        .expect("gcc runs");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "gcc rejected a case: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let temps = regex::Regex::new(r"D\.\d+").unwrap();
    let discarded = regex::Regex::new(r"^\s*\(void\) \d+;$").unwrap();
    let dump = String::from_utf8_lossy(&out.stdout);
    let total = dump.lines().filter(|l| discarded.is_match(l)).count();
    let kept: String = dump
        .lines()
        .filter(|l| !discarded.is_match(l))
        .map(|l| format!("{l}\n"))
        .collect();
    let funcs = bodies(&temps.replace_all(&kept, "D.n"));
    assert!(
        funcs.len() >= 3 * count,
        "the gcc dump is incomplete: {} functions for {count} cases",
        funcs.len()
    );
    let body = |name: &str| {
        funcs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, b)| b.trim().to_string())
            .unwrap_or_default()
    };
    let mut bad = Vec::new();
    let mut chosen = 0;
    for (i, (e, simple)) in cases.iter().enumerate() {
        let (bo, bs) = (body(&format!("o{i}")), body(&format!("s{i}")));
        assert!(!bo.is_empty() && !bs.is_empty(), "no tree for case {i}");
        if bo == bs {
            continue;
        }
        if simple.matches('?').count() < e.matches('?').count() {
            chosen += 1;
        } else {
            bad.push(format!("{e}  =>  {simple}\n--- o{i}\n{bo}\n--- s{i}\n{bs}"));
        }
    }
    eprintln!(
        "{count} expressions, {changed} simplified, {total} discarded constants in the trees, \
         {chosen} constant conditions reduced to one arm (type checked), {} trees differ",
        bad.len()
    );
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

const CONSTANT_CASTS: &[&str] = &[
    "int",
    "long",
    "unsigned",
    "unsigned int",
    "unsigned long",
    "long long",
    "unsigned long long",
    "signed",
    "long unsigned",
    "short",
    "unsigned char",
];
const CONSTANT_BINARY: &[&str] = &[
    "*", "/", "%", "+", "-", "<<", ">>", "<", ">", "<=", ">=", "==", "!=", "&", "^", "|", "&&",
    "||", ",",
];

fn constant(r: &mut Rng, depth: u32) -> String {
    if depth == 0 || r.chance(20) {
        return r.pick(LITERALS).to_string();
    }
    match r.below(17) {
        15 => format!(
            "({}) ? ({}) : (({})f({}))",
            r.pick(LITERALS),
            constant(r, depth - 1),
            r.pick(CONSTANT_CASTS),
            r.pick(LITERALS)
        ),
        16 => format!(
            "({}) ? (({})f({})) : ({})",
            r.pick(LITERALS),
            r.pick(CONSTANT_CASTS),
            r.pick(LITERALS),
            constant(r, depth - 1)
        ),
        10 => match r.below(3) {
            0 => format!("sizeof({})", r.pick(SIZED)),
            1 => format!("sizeof(({}))", constant(r, depth - 1)),
            _ => format!(
                "sizeof(({})({}))",
                r.pick(CONSTANT_CASTS),
                constant(r, depth - 1)
            ),
        },
        11 => format!("{}(({}))", r.pick(BUILTINS), constant(r, depth - 1)),
        12 => format!("__builtin_constant_p(({}))", constant(r, depth - 1)),
        13 => format!(
            "__builtin_choose_expr({}, ({}), ({}))",
            r.pick(LITERALS),
            constant(r, depth - 1),
            constant(r, depth - 1)
        ),
        14 => format!(
            "(__typeof__(({})))({})",
            constant(r, depth - 1),
            constant(r, depth - 1)
        ),
        0..=4 => {
            let op = r.pick(CONSTANT_BINARY);
            let (l, rr) = (constant(r, depth - 1), constant(r, depth - 1));
            format!("({l}) {op} ({rr})")
        }
        5 | 6 => {
            let ty = r.pick(CONSTANT_CASTS);
            format!("({ty})({})", constant(r, depth - 1))
        }
        7 | 8 => {
            let op = r.pick(&["-", "+", "!", "~"]);
            format!("{op}({})", constant(r, depth - 1))
        }
        _ => format!(
            "({}) ? ({}) : ({})",
            constant(r, depth - 1),
            constant(r, depth - 1),
            constant(r, depth - 1)
        ),
    }
}

#[test]
fn folded_constants_keep_their_value_and_type() {
    let Some(count) = std::env::var("KMX_C_EXPR_ORACLE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    else {
        eprintln!("KMX_C_EXPR_ORACLE is not set; skipping the constant folding oracle");
        return;
    };
    let seed: u64 = std::env::var("KMX_C_EXPR_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x9e37_79b9_7f4a_7c15);
    let mut r = Rng(seed.rotate_left(17) | 1);
    let mut src = String::from("int f(int);\n");
    let mut run = String::from("int f(int x) { (void)x; __builtin_trap(); }\nint main(void) {\n");
    let (mut whole, mut partial) = (0, 0);
    for i in 0..count {
        let depth = 1 + r.below(5) as u32;
        let e = constant(&mut r, depth);
        let mut it = Interner::new();
        let toks = lex_all(e.as_bytes(), false, &mut it);
        let Some(folded) = simplify(&toks, &it, &FxHashSet::default(), &LP64) else {
            continue;
        };
        let single = lex_all(folded.as_bytes(), false, &mut it);
        let literal = matches!(single.as_slice(), [t] if t.kind == kmx::syntax::lex::TokenKind::Number)
            || matches!(single.as_slice(), [m, t] if it.get(m.sym) == "-" && t.kind == kmx::syntax::lex::TokenKind::Number);
        let _ = writeln!(
            src,
            "_Static_assert(__builtin_types_compatible_p(__typeof__({e}), __typeof__({folded})), \"type {i}\");"
        );
        if literal && e.contains("f(") {
            whole += 1;
            let _ = writeln!(
                run,
                "  if (({e}) != ({folded})) {{ __builtin_printf(\"value %d\\n\", {i}); return 1; }}"
            );
        } else if literal {
            whole += 1;
            let _ = writeln!(src, "_Static_assert(({e}) == ({folded}), \"value {i}\");");
        } else {
            partial += 1;
        }
    }
    run.push_str("  return 0;\n}\n");
    let dir = std::env::temp_dir().join(format!("kmx-c-const-oracle-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("run.c"), &run).unwrap();
    let built = Command::new("gcc")
        .args(["-w", "-O0", "-std=gnu11", "-o"])
        .arg(dir.join("run"))
        .arg(dir.join("run.c"))
        .output()
        .expect("gcc runs");
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let ran = Command::new(dir.join("run"))
        .output()
        .expect("the check runs");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        ran.status.success(),
        "runtime check failed: {}",
        String::from_utf8_lossy(&ran.stdout)
    );
    let out = Command::new("gcc")
        .args(["-fsyntax-only", "-w", "-std=gnu11", "-x", "c", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(src.as_bytes())?;
            child.wait_with_output()
        })
        .expect("gcc runs");
    let errors = String::from_utf8_lossy(&out.stderr);
    eprintln!(
        "{count} constant expressions: {whole} folded to one literal, {partial} folded in part, {} gcc errors",
        errors.matches("error:").count()
    );
    if !out.status.success() {
        let lines: Vec<&str> = src.lines().collect();
        let shown: Vec<String> = errors
            .lines()
            .filter_map(|l| {
                l.strip_prefix("<stdin>:")?
                    .split(':')
                    .next()?
                    .parse::<usize>()
                    .ok()
            })
            .take(6)
            .filter_map(|n| lines.get(n - 1).map(|l| l.to_string()))
            .collect();
        panic!("{errors}\n{}", shown.join("\n"));
    }
}


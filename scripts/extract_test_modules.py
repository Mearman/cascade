#!/usr/bin/env python3
"""Extract trailing `#[cfg(test)] mod NAME { ... }` blocks from a Rust source
file into sibling files, replacing each in place with a `#[path] mod NAME;`
declaration so the test code keeps full access to the parent module's private
items (the test module stays a child module — only its body moves to a file).

Brace matching is done by a small Rust-aware lexer that ignores braces inside
line comments, (nested) block comments, string/char/raw-string literals, so a
`}` inside a string or a `'}'` char literal never miscounts. `cargo check` after
each run is the loud safety net if anything is mis-parsed.

Usage: extract_test_modules.py <file.rs> [<file.rs> ...]
Only top-level (brace-depth 0) `mod` items immediately preceded by a `#[cfg(test)]`
attribute are moved. Modules already declared with a body-less `mod NAME;` form
are left untouched.
"""
import sys
import os
import re


def scan(src: str):
    """Yield (index, char) for every source char that is *code* (not inside a
    comment or literal). Used for brace matching."""
    i = 0
    n = len(src)
    while i < n:
        c = src[i]
        # Line comment
        if c == '/' and i + 1 < n and src[i + 1] == '/':
            j = src.find('\n', i)
            i = n if j == -1 else j
            continue
        # Block comment (nested)
        if c == '/' and i + 1 < n and src[i + 1] == '*':
            depth = 1
            i += 2
            while i < n and depth > 0:
                if src[i] == '/' and i + 1 < n and src[i + 1] == '*':
                    depth += 1
                    i += 2
                elif src[i] == '*' and i + 1 < n and src[i + 1] == '/':
                    depth -= 1
                    i += 2
                else:
                    i += 1
            continue
        # Raw string: r"...", r#"..."#, br##"..."##
        if c in 'rb':
            m = re.match(r'b?r(#*)"', src[i:])
            if m and (c == 'r' or (c == 'b' and i + 1 < n and src[i + 1] in 'r"')):
                hashes = m.group(1)
                close = '"' + hashes
                j = src.find(close, i + m.end())
                i = n if j == -1 else j + len(close)
                continue
        # Normal / byte string
        if c == '"' or (c == 'b' and i + 1 < n and src[i + 1] == '"'):
            start = i + (2 if c == 'b' else 1)
            j = start
            while j < n:
                if src[j] == '\\':
                    j += 2
                    continue
                if src[j] == '"':
                    break
                j += 1
            i = j + 1
            continue
        # Char literal vs lifetime
        if c == "'":
            # char literal: '\'' , '\n', 'x', '{' ...
            if i + 1 < n and src[i + 1] == '\\':
                j = i + 2
                while j < n and src[j] != "'":
                    j += 1
                i = j + 1
                continue
            if i + 2 < n and src[i + 2] == "'":
                # single-char char literal (the char may be { or })
                i += 3
                continue
            # otherwise a lifetime tick — emit nothing special, advance 1
            i += 1
            continue
        yield i, c
        i += 1


def extract(path: str) -> bool:
    with open(path, 'r') as f:
        src = f.read()

    code = list(scan(src))  # [(idx, char)]
    # Build a quick map of code-brace positions with depth tracking.
    depth = 0
    # Find top-level mod items preceded by #[cfg(test)].
    # Strategy: scan code chars, track depth; when depth==0 and we see the start
    # of `mod <name> {`, record. We detect `mod` by checking the raw source at
    # the code index.
    moves = []  # (attr_start, mod_kw_start, name, body_open_idx, body_close_idx)
    k = 0
    while k < len(code):
        pos, ch = code[k]
        if ch == '{':
            if depth == 0:
                # Is this the body of a top-level `mod NAME {`?
                # Look back over code chars for `mod <ident>` immediately before.
                m = re.search(r'\bmod\s+([A-Za-z_][A-Za-z0-9_]*)\s*$', src[:pos])
                if m:
                    name = m.group(1)
                    mod_kw = m.start()
                    # Require a #[cfg(test)] attribute in the lines just above mod.
                    prefix = src[:mod_kw]
                    # Look at the immediate attribute block preceding `mod`.
                    tail = prefix.rstrip()
                    if has_cfg_test_attr(src, mod_kw):
                        # find matching close brace
                        close = match_close(code, k)
                        if close is not None:
                            attr_start = attr_block_start(src, mod_kw)
                            moves.append((attr_start, mod_kw, name, pos, code[close][0]))
                            depth += 1
                            # skip to close
                            k = close
                            depth -= 1
                            k += 1
                            continue
            depth += 1
        elif ch == '}':
            depth -= 1
        k += 1

    if not moves:
        print(f"  {path}: no top-level #[cfg(test)] mod blocks found")
        return False

    stem = os.path.splitext(os.path.basename(path))[0]
    dirn = os.path.dirname(path)

    # Apply from last to first so indices stay valid.
    out = src
    created = []
    for (attr_start, mod_kw, name, open_idx, close_idx) in sorted(moves, key=lambda x: -x[0]):
        body = out[open_idx + 1:close_idx]
        # Trim one leading/trailing newline for tidiness.
        body = body.strip('\n') + '\n'
        sib_name = f"{stem}_{name}.rs" if name != 'tests' else f"{stem}_tests.rs"
        sib_path = os.path.join(dirn, sib_name)
        header = (
            f"//! Test module for `{os.path.basename(path)}`, split out to keep the\n"
            f"//! parent file under the source-length cap. Declared from there via\n"
            f"//! `#[cfg(test)] #[path = \"{sib_name}\"] mod {name};`, so it stays a child\n"
            f"//! module with full access to the parent's private items.\n\n"
        )
        with open(sib_path, 'w') as f:
            f.write(header + body)
        created.append(sib_path)
        # Replace `[attrs..] mod NAME { body }` region (attr_start..close_idx+1)
        # Preserve attributes EXCEPT we re-emit them, adding #[path].
        attrs = out[attr_start:mod_kw]
        replacement = f"{attrs}#[path = \"{sib_name}\"]\nmod {name};"
        out = out[:attr_start] + replacement + out[close_idx + 1:]

    with open(path, 'w') as f:
        f.write(out)
    print(f"  {path}: moved {len(moves)} module(s) -> {[os.path.basename(c) for c in created]}")
    return True


def has_cfg_test_attr(src: str, mod_kw: int) -> bool:
    """True if the attribute block immediately preceding `mod` contains cfg(test)."""
    start = attr_block_start(src, mod_kw)
    block = src[start:mod_kw]
    return 'cfg(test)' in block.replace(' ', '')


def attr_block_start(src: str, mod_kw: int) -> int:
    """Walk backwards over contiguous attribute lines (#[...]) and blank lines
    directly above `mod` to find where its attribute block begins."""
    # Split preceding text into lines with their offsets.
    text = src[:mod_kw]
    lines = text.split('\n')
    # Reconstruct offsets.
    offsets = []
    off = 0
    for ln in lines:
        offsets.append(off)
        off += len(ln) + 1
    # The last element corresponds to the (partial) line containing `mod`.
    # Walk upward while lines are attributes or blank.
    i = len(lines) - 2  # line above `mod`'s line
    start_line = len(lines) - 1
    while i >= 0:
        s = lines[i].strip()
        if s.startswith('#[') or s.startswith('#![') or s == '' or s.startswith('//'):
            start_line = i
            i -= 1
        else:
            break
    # But don't swallow blank lines that separate from prior code: trim leading
    # blanks of the block.
    while start_line < len(lines) - 1 and lines[start_line].strip() == '':
        start_line += 1
    return offsets[start_line]


def match_close(code, open_k):
    depth = 0
    k = open_k
    while k < len(code):
        ch = code[k][1]
        if ch == '{':
            depth += 1
        elif ch == '}':
            depth -= 1
            if depth == 0:
                return k
        k += 1
    return None


if __name__ == '__main__':
    any_change = False
    for p in sys.argv[1:]:
        if extract(p):
            any_change = True
    sys.exit(0 if any_change else 1)

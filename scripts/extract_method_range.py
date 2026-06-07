#!/usr/bin/env python3
"""Move a contiguous range of methods out of an `impl Type { ... }` block into a
sibling submodule file as a second `impl Type` block, preserving behaviour.

The range runs from the first method named <start_fn> (including its leading
doc-comment / attribute block) to the closing brace of the method named
<end_fn>. The extracted methods are written to <out_file> wrapped in
`use super::*;` + `impl <Type> { ... }`, and removed from the source. The caller
adds the `mod <name>;` declaration separately.

Brace matching uses the same Rust-aware lexer as extract_test_modules.py so a
`}` in a string/char/comment never miscounts. `cargo check` is the safety net.

Usage: extract_method_range.py <file.rs> <Type> <start_fn> <end_fn> <out_file.rs>
"""
import sys
import os
import re


def scan(src: str):
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        if c == '/' and i + 1 < n and src[i + 1] == '/':
            j = src.find('\n', i)
            i = n if j == -1 else j
            continue
        if c == '/' and i + 1 < n and src[i + 1] == '*':
            depth, i = 1, i + 2
            while i < n and depth > 0:
                if src[i] == '/' and i + 1 < n and src[i + 1] == '*':
                    depth, i = depth + 1, i + 2
                elif src[i] == '*' and i + 1 < n and src[i + 1] == '/':
                    depth, i = depth - 1, i + 2
                else:
                    i += 1
            continue
        if c in 'rb':
            m = re.match(r'b?r(#*)"', src[i:])
            if m and (c == 'r' or (c == 'b' and i + 1 < n and src[i + 1] in 'r"')):
                hashes = m.group(1)
                close = '"' + hashes
                j = src.find(close, i + m.end())
                i = n if j == -1 else j + len(close)
                continue
        if c == '"' or (c == 'b' and i + 1 < n and src[i + 1] == '"'):
            j = i + (2 if c == 'b' else 1)
            while j < n:
                if src[j] == '\\':
                    j += 2
                    continue
                if src[j] == '"':
                    break
                j += 1
            i = j + 1
            continue
        if c == "'":
            if i + 1 < n and src[i + 1] == '\\':
                j = i + 2
                while j < n and src[j] != "'":
                    j += 1
                i = j + 1
                continue
            if i + 2 < n and src[i + 2] == "'":
                i += 3
                continue
            i += 1
            continue
        yield i, c
        i += 1


def attr_block_start(src, fn_pos):
    """Walk back over the doc-comment/attribute lines directly above the fn."""
    text = src[:fn_pos]
    lines = text.split('\n')
    offsets, off = [], 0
    for ln in lines:
        offsets.append(off)
        off += len(ln) + 1
    i = len(lines) - 2
    start_line = len(lines) - 1
    while i >= 0:
        s = lines[i].strip()
        if s.startswith('///') or s.startswith('//!') or s.startswith('#[') or s.startswith('#![') or s.startswith('//'):
            start_line, i = i, i - 1
        else:
            break
    return offsets[start_line]


def find_fn(code, src, name):
    """Return the source index of `fn <name>` (the 'f' of fn)."""
    pat = re.compile(r'\bfn\s+' + re.escape(name) + r'\b')
    for m in pat.finditer(src):
        return m.start()
    return None


def match_body_close(code, fn_pos):
    """Given the source pos of `fn name`, find the matching close brace of its body."""
    # find first code '{' at-or-after fn_pos
    depth = 0
    started = False
    for (p, ch) in code:
        if p < fn_pos:
            continue
        if ch == '{':
            depth += 1
            started = True
        elif ch == '}':
            depth -= 1
            if started and depth == 0:
                return p
    return None


def main():
    path, typ, start_fn, end_fn, out_file = sys.argv[1:6]
    with open(path) as f:
        src = f.read()
    code = list(scan(src))

    start_fn_pos = find_fn(code, src, start_fn)
    end_fn_pos = find_fn(code, src, end_fn)
    if start_fn_pos is None or end_fn_pos is None:
        print(f"ERROR: could not find {start_fn} / {end_fn}")
        sys.exit(2)

    range_start = attr_block_start(src, start_fn_pos)
    end_close = match_body_close(code, end_fn_pos)
    if end_close is None:
        print("ERROR: could not match end fn body close")
        sys.exit(2)
    # include to end of that line
    range_end = src.find('\n', end_close)
    range_end = len(src) if range_end == -1 else range_end + 1

    block = src[range_start:range_end]
    dirn = os.path.dirname(path)
    out_path = os.path.join(dirn, out_file)
    header = (
        f"//! `{typ}` methods split out of `{os.path.basename(path)}` to keep that\n"
        f"//! file under the source-length cap. Declared from there via `mod ...;`,\n"
        f"//! so this is a child module of the parent and the methods stay part of\n"
        f"//! the same `impl {typ}` surface with full private access.\n\n"
        f"use super::*;\n\n"
        f"impl {typ} {{\n"
    )
    # The block is a set of methods at 4-space indent; keep as-is inside impl.
    with open(out_path, 'w') as f:
        f.write(header + block.rstrip('\n') + "\n}\n")

    new_src = src[:range_start] + src[range_end:]
    with open(path, 'w') as f:
        f.write(new_src)
    moved_lines = block.count('\n')
    print(f"moved ~{moved_lines} lines ({start_fn}..{end_fn}) -> {out_file}")


if __name__ == '__main__':
    main()

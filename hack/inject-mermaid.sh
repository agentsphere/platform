#!/usr/bin/env bash
# Inject .mmd diagram files into markdown between marker comments.
#
# Markers in .md files:
#   <!-- mermaid:diagrams/context.mmd -->
#   (auto-replaced content)
#   <!-- /mermaid -->
#
# Paths are relative to the .md file's directory.
# Run: hack/inject-mermaid.sh [file ...]
# With no args, processes all docs/arc42/*.md files.

set -euo pipefail

changed=0

inject_file() {
    local md_file="$1"
    local md_dir
    md_dir="$(dirname "$md_file")"
    local tmp
    tmp="$(mktemp)"

    local inside=0
    local mmd_path=""
    local dirty=0

    while IFS= read -r line || [[ -n "$line" ]]; do
        if [[ "$line" =~ ^'<!-- mermaid:'(.+)' -->'$ ]]; then
            mmd_path="${BASH_REMATCH[1]}"
            local full_path="${md_dir}/${mmd_path}"
            echo "$line" >> "$tmp"
            inside=1

            if [[ ! -f "$full_path" ]]; then
                echo "WARNING: $full_path not found (referenced in $md_file)" >&2
                # Keep existing content
                continue
            fi

            # Write the fenced mermaid block
            echo '```mermaid' >> "$tmp"
            cat "$full_path" >> "$tmp"
            echo '```' >> "$tmp"
            dirty=1
            continue
        fi

        if [[ "$line" == '<!-- /mermaid -->' ]]; then
            echo "$line" >> "$tmp"
            inside=0
            continue
        fi

        if [[ $inside -eq 0 ]]; then
            echo "$line" >> "$tmp"
        fi
        # When inside=1, skip old content (replaced above)
    done < "$md_file"

    if [[ $dirty -eq 1 ]]; then
        if ! diff -q "$md_file" "$tmp" > /dev/null 2>&1; then
            cp "$tmp" "$md_file"
            changed=1
            echo "Updated: $md_file"
        fi
    fi
    rm -f "$tmp"
}

if [[ $# -gt 0 ]]; then
    files=("$@")
else
    files=()
    while IFS= read -r -d '' f; do
        files+=("$f")
    done < <(find docs/arc42 -name '*.md' -print0 2>/dev/null)
fi

for f in "${files[@]}"; do
    if grep -q '<!-- mermaid:' "$f" 2>/dev/null; then
        inject_file "$f"
    fi
done

if [[ $changed -eq 1 ]]; then
    # Re-stage updated files so the commit includes injected content
    for f in "${files[@]}"; do
        git add "$f" 2>/dev/null || true
    done
fi

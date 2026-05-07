#!/usr/bin/env bash

set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <pdf-path> <pages>" >&2
  echo "example: $0 ./book.pdf 1,3,5-7" >&2
  exit 1
fi

pdf_path="$1"
pages_spec="$2"
output_dir="outputs"

if [[ ! -f "$pdf_path" ]]; then
  echo "pdf not found: $pdf_path" >&2
  exit 1
fi

mkdir -p "$output_dir"

pdf_stem="$(basename "$pdf_path")"
pdf_stem="${pdf_stem%.*}"

declare -a pages=()

trim() {
  local value="$1"
  value="${value#"${value%%[![:space:]]*}"}"
  value="${value%"${value##*[![:space:]]}"}"
  printf '%s' "$value"
}

append_page() {
  local page="$1"
  if [[ ! "$page" =~ ^[1-9][0-9]*$ ]]; then
    echo "invalid page number: $page" >&2
    exit 1
  fi
  pages+=("$page")
}

IFS=',' read -r -a raw_items <<< "$pages_spec"
for raw_item in "${raw_items[@]}"; do
  item="$(trim "$raw_item")"
  if [[ -z "$item" ]]; then
    continue
  fi

  if [[ "$item" =~ ^([1-9][0-9]*)-([1-9][0-9]*)$ ]]; then
    start="${BASH_REMATCH[1]}"
    end="${BASH_REMATCH[2]}"
    if (( start > end )); then
      echo "invalid page range: $item" >&2
      exit 1
    fi
    for ((page = start; page <= end; page++)); do
      append_page "$page"
    done
    continue
  fi

  append_page "$item"
done

if [[ ${#pages[@]} -eq 0 ]]; then
  echo "no pages specified" >&2
  exit 1
fi

declare -a unique_pages=()
while IFS= read -r page; do
  unique_pages+=("$page")
done < <(printf '%s\n' "${pages[@]}" | sort -n -u)

for page in "${unique_pages[@]}"; do
  temp_dir="$(mktemp -d)"
  prefix="$temp_dir/page"
  output_path="$output_dir/${pdf_stem}-page-${page}.png"

  pdftoppm \
    -f "$page" \
    -l "$page" \
    -png \
    "$pdf_path" \
    "$prefix"

  generated_path="$(find "$temp_dir" -maxdepth 1 -type f -name '*.png' | head -n 1)"
  if [[ -z "$generated_path" ]]; then
    echo "pdftoppm did not produce a PNG for page $page" >&2
    rm -rf "$temp_dir"
    exit 1
  fi

  mv "$generated_path" "$output_path"
  rm -rf "$temp_dir"
  echo "$output_path"
done

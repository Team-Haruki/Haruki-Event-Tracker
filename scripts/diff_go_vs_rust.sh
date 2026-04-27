#!/usr/bin/env bash
# Side-by-side response diff between the live Go tracker API and the
# locally-running Rust port. Compares JSON shape (keys + types, sorted)
# and surfaces any value mismatches that aren't simply timestamps.

set -u
GO=${GO:-http://100.66.113.74:8777}
RS=${RS:-http://127.0.0.1:8777}

# Each line: label<TAB>path<TAB>shape-jq<TAB>value-jq
# value-jq empty means skip value check; path "__DYNAMIC_*__" resolves at runtime.
read -r -d '' CASES <<'EOF' || true
jp/202 status	/event/jp/202/status	keys	.statusDesc
jp/202 ranking-lines	/event/jp/202/ranking-lines	[.[]|keys]|.[0]	[.[].rank]|sort
jp/202 latest rank 1	/event/jp/202/latest-ranking/rank/1	[(.rankData|keys),(.userData|keys)]	.userData
jp/202 latest by uid	__DYNAMIC_LATEST_USER__	[(.rankData|keys),(.userData|keys)]	.userData
jp/202 user-data uid	__DYNAMIC_USER_DATA__	keys	.
jp/202 wl-lines c14	/event/jp/202/world-bloom-ranking-lines/character/14	[.[]|keys]|.[0]	[.[].rank]|sort
jp/202 wl latest c14 r1	/event/jp/202/latest-world-bloom-ranking/character/14/rank/1	[(.rankData|keys),(.userData|keys)]	.userData
jp/202 trace r1 shape	/event/jp/202/trace-ranking/rank/1	[.rankData[0]|keys]|.[0]
jp/202 trace wl c14 r1 shape	/event/jp/202/trace-world-bloom-ranking/character/14/rank/1	[.rankData[0]|keys]|.[0]
jp/202 growth 3600	/event/jp/202/ranking-score-growth/interval/3600	[.[0]|keys]|.[0]
jp/202 wl growth c14 3600	/event/jp/202/world-bloom-ranking-score-growth/character/14/interval/3600	[.[0]|keys]|.[0]
en/164 status	/event/en/164/status	keys	.statusDesc
en/164 ranking-lines	/event/en/164/ranking-lines	[.[]|keys]|.[0]	[.[].rank]|sort
en/164 latest r1	/event/en/164/latest-ranking/rank/1	[(.rankData|keys),(.userData|keys)]
en/164 trace r1 shape	/event/en/164/trace-ranking/rank/1	[.rankData[0]|keys]|.[0]
en/164 growth 3600	/event/en/164/ranking-score-growth/interval/3600	[.[0]|keys]|.[0]
tw/164 status	/event/tw/164/status	keys	.statusDesc
tw/164 ranking-lines	/event/tw/164/ranking-lines	[.[]|keys]|.[0]	[.[].rank]|sort
tw/164 latest r1	/event/tw/164/latest-ranking/rank/1	[(.rankData|keys),(.userData|keys)]
kr/164 status	/event/kr/164/status	keys	.statusDesc
kr/164 ranking-lines	/event/kr/164/ranking-lines	[.[]|keys]|.[0]	[.[].rank]|sort
kr/164 latest r1	/event/kr/164/latest-ranking/rank/1	[(.rankData|keys),(.userData|keys)]
cn/164 status	/event/cn/164/status	keys	.statusDesc
cn/164 ranking-lines	/event/cn/164/ranking-lines	[.[]|keys]|.[0]	[.[].rank]|sort
cn/164 latest r1	/event/cn/164/latest-ranking/rank/1	[(.rankData|keys),(.userData|keys)]
invalid server	/event/zz/202/status	keys	.error
EOF

resolve_dynamic() {
  local raw="$1" base="$2" uid
  case "$raw" in
    __DYNAMIC_LATEST_USER__)
      uid=$(curl -sS --max-time 5 "$base/event/jp/202/latest-ranking/rank/1" | jq -r '.rankData.userId // empty' 2>/dev/null)
      [ -n "$uid" ] && echo "/event/jp/202/latest-ranking/user/$uid"
      ;;
    __DYNAMIC_USER_DATA__)
      uid=$(curl -sS --max-time 5 "$base/event/jp/202/latest-ranking/rank/1" | jq -r '.rankData.userId // empty' 2>/dev/null)
      [ -n "$uid" ] && echo "/event/jp/202/user-data/$uid"
      ;;
    *) echo "$raw" ;;
  esac
}

GREEN='\033[32m'; RED='\033[31m'; YELLOW='\033[33m'; NC='\033[0m'
PASS=0; FAIL=0; SKIP=0

while IFS=$'\t' read -r label path shape_filter value_filter; do
  [ -z "${label:-}" ] && continue

  go_path=$(resolve_dynamic "$path" "$GO")
  rs_path=$(resolve_dynamic "$path" "$RS")
  if [ -z "$go_path" ] || [ -z "$rs_path" ]; then
    printf "%b%-32s%b SKIP  (could not resolve dynamic path)\n" "$YELLOW" "$label" "$NC"
    SKIP=$((SKIP+1))
    continue
  fi

  go_body=$(curl -sS --max-time 8 -w $'\n%{http_code}' "$GO$go_path" 2>/dev/null)
  rs_body=$(curl -sS --max-time 8 -w $'\n%{http_code}' "$RS$rs_path" 2>/dev/null)
  go_status=$(echo "$go_body" | tail -1); go_body=$(echo "$go_body" | sed '$d')
  rs_status=$(echo "$rs_body" | tail -1); rs_body=$(echo "$rs_body" | sed '$d')

  if [ "$go_status" != "$rs_status" ]; then
    printf "%b%-32s%b FAIL  status %s vs %s\n" "$RED" "$label" "$NC" "$go_status" "$rs_status"
    FAIL=$((FAIL+1)); continue
  fi

  # If bodies parse identically (sorted keys), shape and value are trivially equal.
  go_norm=$(echo "$go_body" | jq -cS '.' 2>/dev/null)
  rs_norm=$(echo "$rs_body" | jq -cS '.' 2>/dev/null)
  if [ -n "$go_norm" ] && [ "$go_norm" = "$rs_norm" ]; then
    printf "%b%-32s%b OK    HTTP %s %s (exact)\n" "$GREEN" "$label" "$NC" "$go_status" "$go_path"
    PASS=$((PASS+1)); continue
  fi

  go_shape=$(echo "$go_body" | jq -cS "$shape_filter" 2>&1)
  rs_shape=$(echo "$rs_body" | jq -cS "$shape_filter" 2>&1)
  if echo "$go_shape" | grep -q "jq: error"; then
    printf "%b%-32s%b FAIL  jq-go err\n" "$RED" "$label" "$NC"; echo "    $go_shape"
    FAIL=$((FAIL+1)); continue
  fi
  if echo "$rs_shape" | grep -q "jq: error"; then
    printf "%b%-32s%b FAIL  jq-rs err\n" "$RED" "$label" "$NC"; echo "    $rs_shape"
    FAIL=$((FAIL+1)); continue
  fi

  if [ "$go_shape" != "$rs_shape" ]; then
    printf "%b%-32s%b FAIL  shape mismatch\n" "$RED" "$label" "$NC"
    echo "    Go:   $go_shape"
    echo "    Rust: $rs_shape"
    FAIL=$((FAIL+1)); continue
  fi

  if [ -n "${value_filter:-}" ]; then
    go_val=$(echo "$go_body" | jq -cS "$value_filter" 2>&1)
    rs_val=$(echo "$rs_body" | jq -cS "$value_filter" 2>&1)
    if [ "$go_val" != "$rs_val" ]; then
      printf "%b%-32s%b FAIL  value mismatch\n" "$RED" "$label" "$NC"
      echo "    Go:   $go_val"
      echo "    Rust: $rs_val"
      FAIL=$((FAIL+1)); continue
    fi
  fi

  printf "%b%-32s%b OK    HTTP %s %s\n" "$GREEN" "$label" "$NC" "$go_status" "$go_path"
  PASS=$((PASS+1))
done <<<"$CASES"

echo
echo "PASS=$PASS FAIL=$FAIL SKIP=$SKIP"
[ $FAIL -eq 0 ]

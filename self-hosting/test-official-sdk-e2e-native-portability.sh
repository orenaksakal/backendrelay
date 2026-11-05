#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
script="$script_dir/run-official-sdk-e2e-native.sh"
tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM
mock_bin="$tmp_dir/bin"
mkdir "$mock_bin"

link_command() {
  command_path=$(command -v "$1")
  ln -s "$command_path" "$mock_bin/$1"
}
for command in bash dirname mktemp python3 rm; do
  link_command "$command"
done

for command in cargo curl env git make mise; do
  cat >"$mock_bin/$command" <<'EOF'
#!/bin/sh
exit 0
EOF
  chmod +x "$mock_bin/$command"
done

cat >"$mock_bin/stat" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >>"$tmp_dir/stat.args"
if [ "\$1" = -c ]; then
  exit 1
fi
test "\$1" = -f
test "\$2" = %Lp
printf '%s\n' 600
EOF
chmod +x "$mock_bin/stat"

cat >"$tmp_dir/native.env" <<'EOF'
R2_ACCOUNT_ID=0123456789abcdef0123456789abcdef
R2_ACCESS_KEY_ID=portability-access-sensitive
R2_SECRET_ACCESS_KEY=portability-secret-sensitive
R2_FILES_BUCKET=portability-files
R2_BACKUPS_BUCKET=portability-backups
EOF
chmod 600 "$tmp_dir/native.env"

PATH="$mock_bin" INSTANT_NATIVE_E2E_ENV_FILE="$tmp_dir/native.env" \
  "$script" --preflight >"$tmp_dir/preflight.out" 2>&1 || {
  sed -E "s/portability-(access|secret)-sensitive/[redacted]/g" "$tmp_dir/preflight.out" >&2
  exit 1
}
grep -Fqx -- '-c %a '"$tmp_dir/native.env" "$tmp_dir/stat.args"
grep -Fqx -- '-f %Lp '"$tmp_dir/native.env" "$tmp_dir/stat.args"
grep -Fq "Native official SDK preflight passed for stage 'smoke'" "$tmp_dir/preflight.out"

cat >"$mock_bin/mise" <<'EOF'
#!/bin/sh
trap 'exit 143' TERM
/bin/sleep 30
EOF
chmod +x "$mock_bin/mise"

run_first_bounded_operation() {
  output=$1
  shift
  env PATH="$mock_bin" \
    INSTANT_NATIVE_E2E_ENV_FILE="$tmp_dir/missing.env" \
    INSTANT_NATIVE_E2E_SETUP_TIMEOUT=0.1s \
    R2_ACCOUNT_ID=0123456789abcdef0123456789abcdef \
    R2_ACCESS_KEY_ID=portability-access-sensitive \
    R2_SECRET_ACCESS_KEY=portability-secret-sensitive \
    R2_FILES_BUCKET=portability-files \
    R2_BACKUPS_BUCKET=portability-backups \
    "$@" "$script" >"$output" 2>&1
}

cat >"$mock_bin/timeout" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >"$tmp_dir/timeout.args"
exit 71
EOF
chmod +x "$mock_bin/timeout"
if run_first_bounded_operation "$tmp_dir/timeout.out"; then
  echo 'GNU timeout contract unexpectedly succeeded' >&2
  exit 1
else
  test "$?" -eq 71
fi
grep -Fqx -- '--foreground --signal=TERM --kill-after=15s 0.1s mise install postgres@17.11' \
  "$tmp_dir/timeout.args"

rm "$mock_bin/timeout"
cat >"$mock_bin/gtimeout" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >"$tmp_dir/gtimeout.args"
exit 72
EOF
chmod +x "$mock_bin/gtimeout"
if run_first_bounded_operation "$tmp_dir/gtimeout.out"; then
  echo 'gtimeout contract unexpectedly succeeded' >&2
  exit 1
else
  test "$?" -eq 72
fi
grep -Fqx -- '--foreground --signal=TERM --kill-after=15s 0.1s mise install postgres@17.11' \
  "$tmp_dir/gtimeout.args"

rm "$mock_bin/gtimeout"
if run_first_bounded_operation "$tmp_dir/fallback.out"; then
  echo 'portable timeout fallback unexpectedly succeeded' >&2
  exit 1
else
  test "$?" -eq 124
fi

if grep -R -E 'portability-(access|secret)-sensitive' "$tmp_dir"/*.out >/dev/null; then
  echo 'native SDK portability output exposed a credential' >&2
  exit 1
fi

echo 'Native official SDK portability contract verified'

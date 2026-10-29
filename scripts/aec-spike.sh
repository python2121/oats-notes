#!/usr/bin/env bash
# Echo-cancellation spike: does Apple's voice-processing microphone (VoiceProcessingIO) cancel
# what OTHER apps play through the speakers? Renders the reference sentence with `say -o`,
# records it once with the plain microphone and once through the voice-processing unit
# (crates/oats-audio/examples/aec_spike.rs), and prints the two side by side.
#
#   scripts/aec-spike.sh [--output "MacBook Air Speakers"] [any other aec_spike flag]
#
# Use the built-in speakers (no headphones), ~50 % volume, a quiet room, and stay silent.
# It PLAYS SOUND and opens the microphone: never during a call. Needs the terminal's
# microphone permission. Builds the example in release (only the oats-audio example; the app
# binary is not rebuilt).
#
# Reading it: plain is the control (difference ≥ ~15 dB, clear correlation peaks). VPIO
# cancels other apps' audio when its difference is ≥ 20 dB smaller than plain's and its
# correlation peaks collapse; a few dB means it does not.
set -euo pipefail
cd "$(dirname "$0")/.."

work="$(mktemp -d "${TMPDIR:-/tmp}/aec-spike.XXXXXX")"
trap 'rm -rf "$work"' EXIT

cargo build -q --release -p oats-audio --example aec_spike
bin=target/release/examples/aec_spike
sentence="$("$bin" --print-sentence)"
say -o "$work/ref.wav" --data-format=LEI16@16000 "$sentence"

run() {
  local mode=$1
  shift
  echo "== $mode =="
  "$bin" --mode "$mode" --reference "$work/ref.wav" "$@" | tee "$work/$mode.txt"
  echo
  sleep 2
}
run plain "$@"
run vpio "$@"

field() { grep '^RESULT' "$work/$1.txt" | tr ' ' '\n' | sed -n "s/^$2=//p"; }

printf '%-22s %10s %10s\n' "" plain vpio
for f in room_db play_db diff_db wave_peak env_peak; do
  printf '%-22s %10s %10s\n' "$f" "$(field plain $f)" "$(field vpio $f)"
done

awk -v p="$(field plain diff_db)" -v v="$(field vpio diff_db)" \
    -v pe="$(field plain env_peak)" -v ve="$(field vpio env_peak)" 'BEGIN {
  drop = p - v
  printf "\nplayback-over-room drop with vpio: %.1f dB", drop
  if (pe != "nan" && ve != "nan") printf "; envelope peak %.3f -> %.3f", pe, ve
  printf "\n"
  if (p < 10)        print "VERDICT: inconclusive: the plain control barely heard the speakers (volume? output device?)"
  else if (drop >= 20) print "VERDICT: VoiceProcessingIO CANCELS audio played by other apps (>= 20 dB less playback in the capture)"
  else if (drop >= 6)  print "VERDICT: partial: VoiceProcessingIO reduces audio played by other apps by " sprintf("%.0f", drop) " dB, short of the 20 dB bar"
  else                 print "VERDICT: VoiceProcessingIO does NOT cancel audio played by other apps (keep the text-level echo rule)"
}'

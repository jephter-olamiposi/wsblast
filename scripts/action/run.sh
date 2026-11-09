#!/usr/bin/env bash
# Runs wsblast for the GitHub Action in action.yml. Reads its inputs from
# the environment:
#   WSBLAST_ARGS     the command line after `wsblast`, parsed as a shell line
#   WSBLAST_REPORT   where the JSON report goes, unless the arguments set
#                    --format or -o themselves
#   WSBLAST_SUMMARY  "true" to add the report to the job summary
# Writes the exit-code and report outputs, and exits with wsblast's exit
# code, so an SLO breach (2) fails the step.
set -uo pipefail

# The arguments come from the workflow file, which its author controls.
eval "set -- ${WSBLAST_ARGS:-}"

load_test=1
case "${1:-}" in
  mock | agent | render | import | help | -h | --help | -V | --version) load_test=0 ;;
esac
own_output=0
no_progress=0
for arg in ${1+"$@"}; do
  case "$arg" in
    --format | --format=* | -o | -o* | --output | --output=*) own_output=1 ;;
    --no-progress) no_progress=1 ;;
    -h | --help | -V | --version) load_test=0 ;;
  esac
done

extra=()
report=""
if [ "$load_test" = 1 ]; then
  [ "$no_progress" = 1 ] || extra+=(--no-progress)
  if [ "$own_output" = 0 ]; then
    report="${WSBLAST_REPORT:-wsblast-report.json}"
    extra+=(--format json -o "$report")
  fi
fi

# ${x+"..."} keeps an empty list from tripping set -u in bash before 4.4,
# such as the bash 3.2 of the macOS runners.
wsblast ${1+"$@"} ${extra[0]+"${extra[@]}"}
code=$?

if [ -n "$report" ] && [ -f "$report" ]; then
  echo "report=$report" >> "${GITHUB_OUTPUT:-/dev/null}"
  if [ "${WSBLAST_SUMMARY:-true}" = "true" ] && [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    wsblast render "$report" --format markdown >> "$GITHUB_STEP_SUMMARY" ||
      echo "wsblast: could not add the report to the job summary" >&2
  fi
fi
echo "exit-code=$code" >> "${GITHUB_OUTPUT:-/dev/null}"
case "$code" in
  0) ;;
  2) echo "::error title=wsblast::An SLO or the regression gate failed (exit code 2); the report has the details." ;;
  *) echo "::error title=wsblast::wsblast exited with code $code." ;;
esac
exit "$code"

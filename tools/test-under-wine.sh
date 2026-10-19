#!/usr/bin/env bash
# Run the engine's unit tests on Linux: cross-compile for x86_64-pc-windows-gnu and run the test
# binary under Wine. Wine has no Cloud Files API, so a stub cldapi.dll (every call fails with
# E_NOTIMPL) satisfies the loader; under it every file looks like a plain, non-placeholder file.
# Real cfapi behaviour is covered by the #[ignore] tests on Windows and by CI.
#
# Needs: rustup target x86_64-pc-windows-gnu, mingw-w64 (x86_64-w64-mingw32-gcc), wine64.
# Usage: tools/test-under-wine.sh [test name filter] [-- extra libtest args]
set -euo pipefail
cd "$(dirname "$0")/.."

stub_dir="target/cldapi-stub"
if [[ ! -f "$stub_dir/cldapi.dll" ]]; then
    mkdir -p "$stub_dir"
    {
        echo '/* Test-only stub: every Cf* call fails with E_NOTIMPL. */'
        for n in CfCloseHandle CfConnectSyncRoot CfConvertToPlaceholder CfCreatePlaceholders \
            CfDehydratePlaceholder CfDisconnectSyncRoot CfExecute CfGetCorrelationVector \
            CfGetPlaceholderInfo CfGetPlaceholderRangeInfo CfGetPlaceholderStateFromAttributeTag \
            CfGetPlaceholderStateFromFileInfo CfGetPlaceholderStateFromFindData CfGetPlatformInfo \
            CfGetSyncRootInfoByHandle CfGetSyncRootInfoByPath CfGetTransferKey \
            CfGetWin32HandleFromProtectedHandle CfHydratePlaceholder CfOpenFileWithOplock \
            CfQuerySyncProviderStatus CfReferenceProtectedHandle CfRegisterSyncRoot \
            CfReleaseProtectedHandle CfReleaseTransferKey CfReportProviderProgress \
            CfReportProviderProgress2 CfReportSyncStatus CfRevertPlaceholder CfSetCorrelationVector \
            CfSetInSyncState CfSetPinState CfUnregisterSyncRoot CfUpdatePlaceholder \
            CfUpdateSyncProviderStatus; do
            echo "__declspec(dllexport) long $n(void) { return (long)0x80004001; }"
        done
    } > "$stub_dir/cldapi.c"
    x86_64-w64-mingw32-gcc -shared -O1 -o "$stub_dir/cldapi.dll" "$stub_dir/cldapi.c"
fi

exe=$(cargo test --target x86_64-pc-windows-gnu --no-run --message-format=json 2>/dev/null \
    | python3 -c 'import sys, json
for line in sys.stdin:
    try: m = json.loads(line)
    except ValueError: continue
    if m.get("reason") == "compiler-artifact" and m.get("executable") and m["target"]["kind"] == ["bin"]:
        print(m["executable"])' | tail -1)
if [[ -z "$exe" ]]; then
    cargo test --target x86_64-pc-windows-gnu --no-run   # show the build error
    exit 1
fi

wine=$(command -v wine64 || command -v wine || echo /usr/lib/wine/wine64)
winepath="Z:$(realpath "$stub_dir" | tr / '\\')"
# A UTF-8 locale lets Wine create the non-ASCII names the tests use (Köpmannen, Över).
LANG=C.UTF-8 LC_ALL=C.UTF-8 WINEDEBUG=-all WINEPATH="$winepath" "$wine" "$exe" "$@"

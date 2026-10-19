#!/usr/bin/env python3
"""Validate ui/Package.appxmanifest against the AppX manifest schemas that MakeAppx uses,
without Windows: the schemas are embedded as text in appxpackaging.dll from the
Microsoft.Windows.SDK.BuildTools NuGet package (downloaded once into target/appx-schemas).
MakeAppx reports one error per CI run; this reports them all in a second.

Needs python3 with lxml. Usage: tools/validate-manifest.py [manifest]
"""
import io
import os
import re
import sys
import urllib.request
import zipfile

from lxml import etree

SDK = "10.0.28000.2705"
URL = f"https://api.nuget.org/v3-flatcontainer/microsoft.windows.sdk.buildtools/{SDK}/microsoft.windows.sdk.buildtools.{SDK}.nupkg"
DLL = "bin/10.0.28000.0/x64/appxpackaging.dll"
FOUNDATION = "http://schemas.microsoft.com/appx/manifest/foundation/windows10"

root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
manifest = sys.argv[1] if len(sys.argv) > 1 else os.path.join(root, "ui", "Package.appxmanifest")
cache = os.path.join(root, "target", "appx-schemas", SDK)
os.makedirs(cache, exist_ok=True)

dll = os.path.join(cache, "appxpackaging.dll")
if not os.path.exists(dll):
    print(f"downloading {URL}")
    with urllib.request.urlopen(URL) as r:
        pkg = zipfile.ZipFile(io.BytesIO(r.read()))
    with open(dll, "wb") as f:
        f.write(pkg.read(DLL))

# Every "<?xml ...?> ... </xs:schema>" in the DLL is one schema; keep the first per namespace.
raw = open(dll, "rb").read()
start, schemas = 0, {}
while (s := raw.find(b'<?xml version="1.0" encoding="utf-8"?>', start)) >= 0:
    e = raw.find(b"</xs:schema>", s)
    nxt = raw.find(b"<?xml version", s + 10)
    if e < 0:
        break
    if 0 <= nxt < e:
        start = nxt
        continue
    text = raw[s : e + len(b"</xs:schema>")].decode("utf-8", "replace")
    if m := re.search(r'targetNamespace="([^"]+)"', text):
        schemas.setdefault(m.group(1), text)
    start = e

names = {ns: f"s{i}.xsd" for i, ns in enumerate(sorted(schemas))}
for ns, text in schemas.items():
    head_end = text.find(">", text.find("<xs:schema")) + 1
    text = re.sub(
        r'<xs:import namespace="([^"]+)"\s*/>',
        lambda m: f'<xs:import namespace="{m.group(1)}" schemaLocation="{names[m.group(1)]}"/>' if m.group(1) in names else m.group(0),
        text,
    )
    # MakeAppx resolves references to namespaces a schema declares but never imports.
    head_end = text.find(">", text.find("<xs:schema")) + 1
    declared = set(re.findall(r'xmlns:\w+="([^"]+)"', text[:head_end]))
    imported = set(re.findall(r'<xs:import namespace="([^"]+)"', text))
    extra = "".join(f'<xs:import namespace="{d}" schemaLocation="{names[d]}"/>' for d in sorted(declared) if d in names and d != ns and d not in imported)
    text = text[:head_end] + extra + text[head_end:]
    # libxml2 is stricter than MakeAppx: no stray spaces in QNames, no \x escapes, maxOccurs
    # below 2^31.
    text = re.sub(r'\b(type|ref|base|substitutionGroup)="\s*([^"]*?)\s*"', r'\1="\2"', text)
    text = text.replace(r"\x01-\x1f", r"\p{Cc}")
    text = re.sub(r'maxOccurs="\d{10,}"', 'maxOccurs="unbounded"', text)
    open(os.path.join(cache, names[ns]), "w", encoding="utf-8").write(text)

doc_text = open(manifest, encoding="utf-8-sig").read()
used = set(re.findall(r'xmlns(?::\w+)?="([^"]+)"', doc_text)) | {FOUNDATION}
wrapper = '<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" targetNamespace="urn:synka-wrapper">'
wrapper += "".join(f'<xs:import namespace="{ns}" schemaLocation="{names[ns]}"/>' for ns in sorted(used) if ns in names)
wrapper += "</xs:schema>"
open(os.path.join(cache, "all.xsd"), "w").write(wrapper)

# Patterns libxml2 cannot compile (\b, look-ahead) are loosened to ".*", and said so.
for _ in range(100):
    try:
        schema = etree.XMLSchema(etree.parse(os.path.join(cache, "all.xsd")))
        break
    except etree.XMLSchemaParseError as e:
        m = re.search(r"The value '(.*)' of the facet 'pattern' is not a valid regular expression", str(e))
        if not m:
            raise
        bad = m.group(1)
        print(f"note: pattern not checked: {bad}")
        esc = bad.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;").replace('"', "&quot;")
        for f in os.listdir(cache):
            if f.endswith(".xsd"):
                p = os.path.join(cache, f)
                t = open(p, encoding="utf-8").read()
                t2 = t.replace(f'value="{esc}"', 'value=".*"').replace(f'value="{bad}"', 'value=".*"')
                if t2 != t:
                    open(p, "w", encoding="utf-8").write(t2)

# MSBuild fills in $targetentrypoint$.
doc = etree.fromstring(doc_text.replace("$targetentrypoint$", "Synka.App").encode("utf-8"))
if schema.validate(doc):
    print(f"{os.path.relpath(manifest, root)}: valid")
    sys.exit(0)
for e in schema.error_log:
    print(f"{os.path.relpath(manifest, root)}:{e.line}: {e.message}")
sys.exit(1)

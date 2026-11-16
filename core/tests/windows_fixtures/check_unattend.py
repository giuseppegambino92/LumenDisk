"""Parse an autounattend.xml with Python's XML parser (an independent check of what
LumenDisk generated) and print, as JSON, what Windows Setup would read from it:
  python3 -I check_unattend.py autounattend.xml
"""
import base64
import json
import sys
import xml.etree.ElementTree as ET

NS = {"u": "urn:schemas-microsoft-com:unattend"}
WCM = "{http://schemas.microsoft.com/WMIConfig/2002/State}"

root = ET.parse(sys.argv[1]).getroot()
assert root.tag == "{urn:schemas-microsoft-com:unattend}unattend", root.tag
out = {"passes": {}, "labconfig": [], "specialize": [], "accounts": [], "oobe": {}, "international": {}, "time_zone": None, "first_logon": []}


def text(el, path):
    e = el.find(path, NS)
    return None if e is None else (e.text or "")


for settings in root.findall("u:settings", NS):
    p = settings.get("pass")
    comps = out["passes"].setdefault(p, [])
    for c in settings.findall("u:component", NS):
        name, arch = c.get("name"), c.get("processorArchitecture")
        comps.append([name, arch])
        assert c.get("publicKeyToken") == "31bf3856ad364e35"
        for cmd in c.findall("u:RunSynchronous/u:RunSynchronousCommand", NS):
            assert cmd.get(WCM + "action") == "add"
            path = text(cmd, "u:Path")
            if p == "windowsPE":
                # reg add HKLM\SYSTEM\Setup\LabConfig /v NAME /t REG_DWORD /d 1 /f
                w = path.split()
                assert w[:3] == ["reg", "add", "HKLM\\SYSTEM\\Setup\\LabConfig"], path
                assert w[3] == "/v" and w[5:] == ["/t", "REG_DWORD", "/d", "1", "/f"], path
                out["labconfig"].append(w[w.index("/v") + 1])
            else:
                out["specialize"].append(path)
        for acc in c.findall("u:UserAccounts/u:LocalAccounts/u:LocalAccount", NS):
            raw = base64.b64decode(text(acc, "u:Password/u:Value")).decode("utf-16-le")
            assert raw.endswith("Password"), raw
            out["accounts"].append({
                "name": text(acc, "u:Name"),
                "group": text(acc, "u:Group"),
                "plain_text": text(acc, "u:Password/u:PlainText"),
                "password": raw[: -len("Password")],
            })
        oobe = c.find("u:OOBE", NS)
        if oobe is not None:
            for k in oobe:
                out["oobe"][k.tag.split("}")[1]] = k.text
        for k in ("InputLocale", "SystemLocale", "UILanguage", "UserLocale"):
            v = text(c, "u:" + k)
            if v is not None:
                out["international"][k] = v
        tz = text(c, "u:TimeZone")
        if tz is not None:
            out["time_zone"] = tz
        for cmd in c.findall("u:FirstLogonCommands/u:SynchronousCommand", NS):
            out["first_logon"].append(text(cmd, "u:CommandLine"))
print(json.dumps(out, ensure_ascii=False))

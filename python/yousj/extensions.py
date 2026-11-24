"""Extension support for the Yousj Browser (real Chromium backend).

Installs, lists and removes browser extensions, and tells the browser
service which directories to pass to Chromium as --load-extension.

Supported sources:
  * an unpacked extension directory (manifest.json at its root)
  * a Chrome .crx package
  * a Firefox .xpi package (it is a zip; the manifest is normalized by
    dropping Gecko-only keys -- add-ons that use Firefox-only APIs
    still will not run, and the install result says so)
  * the Chrome Web Store, by extension id: the .crx is fetched from
    Google's update endpoint, unpacked, and loaded the same way.

Layout (all under ~/.config/yousj-browser/):
  extensions.json          registry: id -> {name, version, source, ...}
  extensions/<id>/         the unpacked extension files

Chromium computes an extension's real id from its key/path; the
registry id here is our own stable slug (store id when installed from
the store, else a name+hash slug).
"""
import hashlib
import json
import os
import shutil
import struct
import urllib.parse
import urllib.request
import zipfile

BASE = os.path.join(os.path.expanduser("~"), ".config", "yousj-browser")
EXT_DIR = os.path.join(BASE, "extensions")
REGISTRY = os.path.join(BASE, "extensions.json")

STORE_URL = ("https://clients2.google.com/service/update2/crx"
             "?response=redirect&prodversion=158.0.8127.36"
             "&acceptformat=crx2,crx3&x=id%3D{ext_id}%26uc")


def _load_registry():
    try:
        with open(REGISTRY) as f:
            return json.load(f)
    except Exception:
        return {}


def _save_registry(reg):
    os.makedirs(BASE, mode=0o700, exist_ok=True)
    tmp = REGISTRY + ".tmp"
    with open(tmp, "w") as f:
        json.dump(reg, f, ensure_ascii=False, indent=1)
    os.replace(tmp, REGISTRY)


def _slug(text):
    s = "".join(c if (c.isalnum() or c in "-_") else "-"
                for c in str(text).lower()).strip("-")
    return s or "ext"


def _crx_to_zip(data):
    if data[:4] != b"Cr24":
        raise ValueError("not a CRX file (bad magic)")
    version = struct.unpack("<I", data[4:8])[0]
    if version == 3:
        header_len = struct.unpack("<I", data[8:12])[0]
        return data[12 + header_len:]
    if version == 2:
        pubkey_len, sig_len = struct.unpack("<II", data[8:16])
        return data[16 + pubkey_len + sig_len:]
    raise ValueError("unsupported CRX version %d" % version)


def _extract_zip_bytes(data, dest):
    import io
    with zipfile.ZipFile(io.BytesIO(data)) as z:
        for name in z.namelist():
            if name.startswith("/") or ".." in name.split("/"):
                raise ValueError("unsafe path in package: %s" % name)
        z.extractall(dest)


def _normalize_manifest(ext_dir):
    """Read manifest.json; strip Gecko-only keys so Chromium accepts
    Firefox packages. Returns (manifest, notes)."""
    path = os.path.join(ext_dir, "manifest.json")
    with open(path) as f:
        manifest = json.load(f)
    notes = []
    changed = False
    for key in ("browser_specific_settings", "applications"):
        if key in manifest:
            del manifest[key]
            changed = True
            notes.append("removed Gecko-only key: %s" % key)
    if manifest.get("manifest_version") == 2:
        notes.append("Manifest V2: modern Chromium may refuse or "
                     "limit MV2 extensions")
    bg = manifest.get("background") or {}
    if isinstance(bg, dict) and bg.get("scripts"):
        notes.append("uses background.scripts (Firefox/MV2 style); "
                     "Chromium MV3 needs a service_worker, not converted")
    return manifest, notes, changed


def _install_tree(src_dir, source, forced_id=None):
    manifest_path = os.path.join(src_dir, "manifest.json")
    if not os.path.isfile(manifest_path):
        raise ValueError("no manifest.json at the package root")
    manifest, notes, changed = _normalize_manifest(src_dir)
    if changed:
        with open(manifest_path, "w") as f:
            json.dump(manifest, f, ensure_ascii=False, indent=1)
    name = manifest.get("name") or "extension"
    if isinstance(name, dict):
        name = name.get("message") or "extension"
    if str(name).startswith("__MSG_"):
        notes.append("localized name (%s); shown as-is" % name)
    version = str(manifest.get("version") or "")
    if forced_id:
        ext_id = forced_id
    else:
        digest = hashlib.sha256(
            (str(name) + "|" + version).encode()).hexdigest()[:8]
        ext_id = "%s-%s" % (_slug(name), digest)
    dest = os.path.join(EXT_DIR, ext_id)
    os.makedirs(EXT_DIR, mode=0o700, exist_ok=True)
    if os.path.isdir(dest):
        shutil.rmtree(dest)
    shutil.copytree(src_dir, dest)
    reg = _load_registry()
    reg[ext_id] = {"name": name, "version": version,
                   "manifest_version": manifest.get("manifest_version"),
                   "source": source, "enabled": True, "notes": notes}
    _save_registry(reg)
    return ext_id, reg[ext_id]


def install_from_path(path):
    """Install from a directory, .crx, .xpi or .zip path."""
    path = os.path.expanduser(path)
    if os.path.isdir(path):
        return _install_tree(path, "dir:%s" % path)
    if not os.path.isfile(path):
        raise ValueError("not found: %s" % path)
    lower = path.lower()
    with open(path, "rb") as f:
        data = f.read()
    if lower.endswith(".crx"):
        data = _crx_to_zip(data)
        kind = "crx"
    elif lower.endswith((".xpi", ".zip")):
        kind = "xpi" if lower.endswith(".xpi") else "zip"
    else:
        raise ValueError("unsupported package type: %s" % path)
    tmp = os.path.join(BASE, "tmp-unpack")
    if os.path.isdir(tmp):
        shutil.rmtree(tmp)
    os.makedirs(tmp)
    try:
        _extract_zip_bytes(data, tmp)
        return _install_tree(tmp, "%s:%s" % (kind, path))
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def install_from_store(ext_id):
    """Download an extension's .crx from the Chrome Web Store update
    endpoint and install it."""
    ext_id = ext_id.strip()
    url = STORE_URL.format(ext_id=urllib.parse.quote(ext_id))
    req = urllib.request.Request(url, headers={
        "User-Agent": "Mozilla/5.0 (X11; Linux x86_64) "
                      "AppleWebKit/537.36 Chrome/158.0 Safari/537.36"})
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            data = resp.read()
    except Exception as e:
        raise ValueError("Chrome Web Store unreachable from this "
                         "network (%s); install from a .crx file "
                         "instead" % e)
    if data[:4] != b"Cr24":
        raise ValueError("store did not return a CRX package "
                         "(%d bytes)" % len(data))
    tmp = os.path.join(BASE, "tmp-unpack")
    if os.path.isdir(tmp):
        shutil.rmtree(tmp)
    os.makedirs(tmp)
    try:
        _extract_zip_bytes(_crx_to_zip(data), tmp)
        return _install_tree(tmp, "store:%s" % ext_id, forced_id=ext_id)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def list_extensions():
    reg = _load_registry()
    return [{"id": k, **v} for k, v in sorted(reg.items())]


def set_enabled(ext_id, enabled):
    reg = _load_registry()
    if ext_id not in reg:
        raise ValueError("unknown extension: %s" % ext_id)
    reg[ext_id]["enabled"] = bool(enabled)
    _save_registry(reg)
    return reg[ext_id]


def remove(ext_id):
    reg = _load_registry()
    if ext_id not in reg:
        raise ValueError("unknown extension: %s" % ext_id)
    del reg[ext_id]
    _save_registry(reg)
    dest = os.path.join(EXT_DIR, ext_id)
    if os.path.isdir(dest):
        shutil.rmtree(dest)
    return True


def enabled_dirs():
    reg = _load_registry()
    out = []
    for ext_id, info in sorted(reg.items()):
        if info.get("enabled", True):
            dest = os.path.join(EXT_DIR, ext_id)
            if os.path.isdir(dest):
                out.append(dest)
    return out

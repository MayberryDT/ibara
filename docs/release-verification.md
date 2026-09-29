# Release verification

Every ibara release is signed. The one-line installer and `ibara update` check the signature and every package before installing anything, so you do not need to do this by hand. This page is for when you want to see the checks for yourself, or to check a release before you run its installer.

## What a release contains

Each release on the [releases page](https://github.com/MayberryDT/ibara/releases) has exactly 7 files:

| File | What it is |
|---|---|
| `install` | The one-line installer, with the release address and public key written into it |
| `stable.json` | The manifest: the version, each package's SHA-256 and size, the commits it was built from, and the release notes |
| `stable.json.sig` | The manifest's SSH signature |
| `ibara-VERSION-x86_64.pkg.tar.zst` | The `ibara` package |
| `ibara-stream-VERSION-x86_64.pkg.tar.zst` | The streaming host for Take Control |
| `ibara-view-VERSION-x86_64.pkg.tar.zst` | The viewer for Take Control |
| `ibara-VERSION-source.tar.gz` | The full source of all 3 packages |

## The release key

Releases are signed with one Ed25519 SSH key. Its public half is in [packaging/release.env](../packaging/release.env), and it is built into every `ibara` program and installer:

```text
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHxVwzeqzB+dhtiZ/s8LW6cPPHKbxNi2m6/XLVTQdMjE ibara-release
```

Its fingerprint is `SHA256:5HvY/YcgalTemE4XlIg0Gx1yo70Y7OdGhmV0EHtZXys`.

## Check a release by hand

You need `curl`, `jq`, `sha256sum` and `ssh-keygen` (from `openssh`).

1. Download the manifest, its signature and the packages into an empty folder. Replace `VERSION` with the release's version, for example `0.1.0-1`, or use `latest/download` for the newest release:

   ```bash
   base=https://github.com/MayberryDT/ibara/releases/download/vVERSION
   curl -fsSL -O "$base/stable.json" -O "$base/stable.json.sig"
   for file in $(jq -r '.packages[].file, .source.file' stable.json); do curl -fsSLO "$base/$file"; done
   ```

2. Check the manifest's signature against the release key:

   ```bash
   printf 'ibara-release namespaces="ibara-release" %s\n' \
     'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHxVwzeqzB+dhtiZ/s8LW6cPPHKbxNi2m6/XLVTQdMjE ibara-release' >allowed_signers
   ssh-keygen -Y verify -f allowed_signers -I ibara-release -n ibara-release -s stable.json.sig <stable.json
   ```

   It prints a line starting with `Good "ibara-release" signature` when the manifest is exactly what the key signed.

3. Check each package against the manifest:

   ```bash
   jq -r '.packages[] | "\(.sha256)  \(.file)"' stable.json | sha256sum -c
   ```

   Each package should say `OK`.

4. See which commits the release was built from:

   ```bash
   jq .source stable.json
   ```

   `core` and `plugin` are commits of [MayberryDT/ibara](https://github.com/MayberryDT/ibara) and [MayberryDT/omarchy-ibara](https://github.com/MayberryDT/omarchy-ibara). `stream` and `view` are commits of the 2 forks, whose source is in the source tarball.

The manifest does not list the source tarball's digest. It is built from the same commits as the packages, and you can compare its `core/` and `omarchy-ibara/` folders with those commits.

## What the installer checks

The `install` script makes the same checks before it installs anything:

1. It downloads `stable.json` and `stable.json.sig` over HTTPS, following only HTTPS redirects.
2. It checks the signature with the key written into it.
3. It downloads the 3 packages the manifest names for that version, and checks each one's SHA-256.
4. Only then does it install all 3 packages with one `pacman -U`, and run `ibara setup`.

`ibara update` makes the same checks with the key built into the installed `ibara`. It refuses a release whose signature or digests do not match, and installs only a release newer than the one installed.

You can read the installer before running it:

```bash
curl -fsSL https://github.com/MayberryDT/ibara/releases/latest/download/install | less
```

## If a check fails

Do not install that release. Check that you downloaded all files from the same release, then [open an issue](https://github.com/MayberryDT/ibara/issues/new/choose). If you think a release was tampered with, report it privately as [SECURITY.md](../SECURITY.md) describes.

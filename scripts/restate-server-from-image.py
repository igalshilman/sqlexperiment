#!/usr/bin/env python3
"""Copy restate-server out of a Restate container image, without docker.

    scripts/restate-server-from-image.py ghcr.io/restatedev/restate:service-protocol-v8 out

Pulls the image's linux/amd64 (or arm64, on such a machine) manifest from the
registry anonymously, then reads its layers, newest first, until one holds
usr/local/bin/restate-server. The binary is linked against glibc: Linux only.
"""

import json
import platform
import sys
import tarfile
import urllib.request

BINARY = "usr/local/bin/restate-server"
ACCEPT = ",".join(
    [
        "application/vnd.oci.image.index.v1+json",
        "application/vnd.docker.distribution.manifest.list.v2+json",
        "application/vnd.oci.image.manifest.v1+json",
        "application/vnd.docker.distribution.manifest.v2+json",
    ]
)


def main(image: str, out: str) -> None:
    registry, _, rest = image.partition("/")
    repository, _, tag = rest.partition(":")
    token = json.load(
        urllib.request.urlopen(f"https://{registry}/token?scope=repository:{repository}:pull")
    )["token"]

    def get(path: str):
        request = urllib.request.Request(
            f"https://{registry}/v2/{repository}/{path}",
            headers={"Authorization": f"Bearer {token}", "Accept": ACCEPT},
        )
        return urllib.request.urlopen(request)

    manifest = json.load(get(f"manifests/{tag or 'latest'}"))
    if "manifests" in manifest:
        arch = {"x86_64": "amd64", "aarch64": "arm64", "arm64": "arm64"}[platform.machine()]
        digest = next(
            entry["digest"]
            for entry in manifest["manifests"]
            if entry["platform"]["architecture"] == arch and entry["platform"]["os"] == "linux"
        )
        manifest = json.load(get(f"manifests/{digest}"))

    for layer in reversed(manifest["layers"]):
        with tarfile.open(fileobj=get(f"blobs/{layer['digest']}"), mode="r|*") as tar:
            for member in tar:
                if member.name.lstrip("./") == BINARY:
                    with open(out, "wb") as file:
                        file.write(tar.extractfile(member).read())
                    return
    sys.exit(f"{image} has no {BINARY}")


if __name__ == "__main__":
    main(*sys.argv[1:])

"""Entry point for `python -m himalaya_doc_service`."""

import sys
import os

# Ensure the package root is on sys.path so submodules resolve
_pkg_dir = os.path.dirname(os.path.abspath(__file__))
_parent = os.path.dirname(_pkg_dir)
if _parent not in sys.path:
    sys.path.insert(0, _parent)

from himalaya_doc_service.server import McpServer


def main():
    server = McpServer()
    server.run()


if __name__ == "__main__":
    main()

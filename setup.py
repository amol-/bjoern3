import os
import glob
from setuptools import setup, Extension

# Intent: centralize build-time options so packagers can control signal handling via env vars.
# Result: compile-time macros are defined consistently based on optional environment overrides.
WANT_SIGINT_HANDLING = os.environ.get("BJOERN_WANT_SIGINT_HANDLING", True)
WANT_SIGNAL_HANDLING = os.environ.get("BJOERN_WANT_SIGNAL_HANDLING", True)
SIGNAL_CHECK_INTERVAL = os.environ.get("BJOERN_SIGNAL_CHECK_INTERVAL", "0.1")

compile_flags = [("SIGNAL_CHECK_INTERVAL", SIGNAL_CHECK_INTERVAL)]
if WANT_SIGNAL_HANDLING:
    compile_flags.append(("WANT_SIGNAL_HANDLING", "yes"))
if WANT_SIGINT_HANDLING:
    compile_flags.append(("WANT_SIGINT_HANDLING", "yes"))

# Intent: collect C sources for the extension in one place to keep the build definition concise.
# Result: the extension compiles both the vendored HTTP parser and bjoern C sources.
SOURCE_FILES = [os.path.join("http-parser", "http_parser.c")] + sorted(
    glob.glob(os.path.join("bjoern", "*.c"))
)

bjoern_extension = Extension(
    "_bjoern",
    sources=SOURCE_FILES,
    libraries=["ev"],
    include_dirs=[
        "http-parser",
        "/usr/include/libev",
        "/opt/local/include",
        "/opt/homebrew/include",
        "/usr/local/include",
    ],
    library_dirs=["/opt/homebrew/lib/", "/usr/local/lib"],
    define_macros=compile_flags,
    extra_compile_args=[
        "-std=c99",
        "-fno-strict-aliasing",
        "-fcommon",
        "-fPIC",
        "-Wall",
        "-Wextra",
        "-Wno-unused-parameter",
        "-Wno-missing-field-initializers",
        "-g",
    ],
)

# Intent: delegate project metadata to pyproject.toml while keeping extension build logic here.
# Result: PEP 517 builds pick up metadata from pyproject.toml with this extension configured.
setup(ext_modules=[bjoern_extension])

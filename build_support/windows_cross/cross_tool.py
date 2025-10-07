#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

'''
Runs an LLVM tool (such as clang-cl) with the correct environment variables set
for cross-compiling on Windows. Locates the necessary library and include paths
from the Windows SDK and Visual Studio.

This script must be linked to the desired tool name with the architecture
prefixed, e.g. x86_64-clang-cl or aarch64-lld-link. It will then find the
correct paths for the tool and run it.

This can only be run from within WSL2.
'''

import subprocess
import json
import os
import sys
import tempfile
import glob
import argparse
import re

tools = ['clang-cl', 'lld-link', 'llvm-lib', 'llvm-dlltool', 'llvm-rc', 'midlrt.exe']


def wslpath(p):
    return subprocess.check_output(['wslpath', p]).decode('utf-8').strip()


def reg(p):
    output = subprocess.check_output(
        ['reg.exe', 'query', p, '/v', 'KitsRoot10']).decode('utf-8')
    # Looks like
    # HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows Kits\Installed Roots
    #     KitsRoot10    REG_SZ    C:\Program Files (x86)\Windows Kits\10\
    # Get the C:\Program Files (x86)\Windows Kits\10\ part
    return output.split('\n')[2].split(maxsplit=2)[2]


def vs_paths(arch):
    try:
        if arch == 'x64':
            component = "Microsoft.VisualStudio.Component.VC.Tools.x86.x64"
        elif arch == 'arm64':
            component = "Microsoft.VisualStudio.Component.VC.Tools.ARM64"
        else:
            raise Exception("Unknown architecture")
        vswhere = wslpath(
            'C:\\Program Files (x86)\\Microsoft Visual Studio\\Installer\\vswhere.exe')
        info = json.loads(subprocess.check_output(
            [vswhere, '-latest', '-products', '*', '-requires', component, '-format', 'json']).decode('utf-8').strip())[0]
        install_path = wslpath(info['installationPath'])
        version_file = f'{install_path}/VC/Auxiliary/Build/Microsoft.VCToolsVersion.default.txt'
        with open(version_file) as f:
            v = f.read().strip()
        msvc_path = f'{install_path}/VC/Tools/MSVC/{v}'
        lib = [f'{msvc_path}/lib/{arch}']
        include = [f'{msvc_path}/include']
        return {'lib': lib, 'include': include}
    except:
        raise Exception("Visual Studio not found")


def sdk_paths(arch):
    roots = wslpath(
        reg('HKLM\\SOFTWARE\\Microsoft\\Windows Kits\\Installed Roots'))
    versions = os.listdir(f'{roots}/Lib')
    pattern = re.compile(r'^[\d.]+$')
    versions = [v for v in versions if pattern.fullmatch(v)]
    versions.sort()
    version = versions[-1]
    lib = [f'{roots}/Lib/{version}/{dir}/{arch}' for dir in ['ucrt', 'um']]
    include = [f'{roots}/Include/{version}/{dir}' for dir in ['ucrt', 'um', 'shared', 'cppwinrt', 'winrt']]
    bin = f'{roots}/bin/{version}/{arch}/'
    return {'lib': lib, 'include': include, 'bin': bin}


def check_config(a):
    try:
        if not all(os.path.isfile(path) for path in a['tools'].values() if path is not None):
            return False
        if not all(os.path.isdir(p) for p in a['lib'] + a['include']):
            return False
        return True
    except:
        return False


def find_llvm_tool(name):
    paths = os.environ.get("PATH").split(os.pathsep)
    for p in paths:
        if os.path.isfile(f'{p}/{name}'):
            return f'{p}/{name}'
        x = glob.glob(f'{p}/{name}-*')
        if len(x) == 0:
            continue
        x.sort()
        return x[-1]
    return None


def find_midlrt(sdk):
    midlrt = f"{sdk['bin']}/midlrt.exe"
    return os.path.normpath(midlrt)


def get_config(arch, required_tool, ignore_cache):
    cache_dir = os.environ.get(
        'XDG_CACHE_HOME', os.path.expanduser('~/.cache'))
    cache_dir = f'{cache_dir}/windows-cross'
    os.makedirs(cache_dir, exist_ok=True)
    cache_file = f'{cache_dir}/cross-{arch}.json'

    config = None
    if not ignore_cache:
        try:
            mtime = os.path.getmtime(cache_file)
            # If the mtime is older than this script, don't trust it.
            if mtime > os.path.getmtime(__file__):
                with open(cache_file) as f:
                    data = json.load(f)
                if check_config(data):
                    config = data
        except:
            pass

    if config is None or (required_tool and required_tool not in config['tools']):
        if arch == 'x86_64':
            win_arch = 'x64'
        elif arch == 'aarch64':
            win_arch = 'arm64'
        else:
            raise Exception("Unknown architecture")
        sdk = sdk_paths(win_arch)
        tool_paths = {}
        config = {}
        for tool in tools:
            if tool == 'midlrt.exe':
                p = find_midlrt(sdk)
            else:
                p = find_llvm_tool(tool)
            if p:
                tool_paths[tool] = p

        if required_tool and required_tool not in tool_paths:
            raise Exception(
                f"tool '{required_tool}' not found, try installing it")

        vs = vs_paths(win_arch)
        config = {'lib': [os.path.normpath(p) for p in vs['lib'] + sdk['lib']],
                  'include': [os.path.normpath(p) for p in vs['include'] + sdk['include']],
                  'tools': tool_paths,
                  'sdk': [os.path.normpath(sdk['bin'])]}

        if not check_config(config):
            raise Exception("invalid paths")

        [f, p] = tempfile.mkstemp(dir=cache_dir)
        with os.fdopen(f, 'w') as f:
            json.dump(config, f)
        os.rename(p, cache_file)

    return config


name = os.path.basename(sys.argv[0])
arch = None
ignore_cache = False
action = "run"
tool = None
tool_args = sys.argv[1:]
for t in tools:
    if name.endswith(f"-{t}"):
        tool = t
        break

if not tool:
    parser = argparse.ArgumentParser()
    parser.add_argument('--arch', choices=['x86_64', 'aarch64'], required=True)
    parser.add_argument('--ignore-cache', action='store_true')
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument('--tool', choices=tools)
    group.add_argument('--dump', action='store_true')
    group.add_argument('--install', action='store_true')
    parser.add_argument('args', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.dump:
        action = "dump"
    elif args.install:
        action = "install"
    arch = args.arch
    tool = args.tool
    ignore_cache = args.ignore_cache
    tool_args = args.args

if not arch:
    if name.startswith('x86_64-'):
        arch = 'x86_64'
    elif name.startswith('aarch64-'):
        arch = 'aarch64'
    else:
        print("unknown arch")
        exit(1)

config = get_config(arch, tool, ignore_cache)

if action == "run":
    tool_path = config['tools'][tool]
    separator = ':' if tool == "midlrt.exe" else ';'
    lib = separator.join(config['lib'])
    include = separator.join(config['include'])
    environ = dict(os.environ.copy(), LIB=lib, INCLUDE=include)
    if tool == "midlrt.exe":
        wslenv = environ['WSLENV']
        if wslenv is None:
            wslenv = ""
        wslenv = wslenv + ":INCLUDE/wl:LIB/wl"
        environ['WSLENV'] = wslenv

    os.execvpe(tool_path, [tool_path] + tool_args, environ)
elif action == "dump":
    print(json.dumps(config))
elif action == "install":
    dir = os.path.dirname(__file__)
    script = os.path.basename(__file__)
    for tool in tools:
        dst = f'{dir}/{arch}-{tool}'
        if os.path.islink(dst):
            os.unlink(dst)
        os.symlink(script, dst)

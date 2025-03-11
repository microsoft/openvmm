#!/usr/bin/python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

#
# There is a similar tool implemented in C inside the Linux kernel source tree.
# The goal for this one has been to be independent of the kernel source tree,
# and be able to create the initial RAM FS files without running as root.
#
# The format for the configuration file lines:
#
# "file <name> <location> <mode> <uid> <gid> [<hard links>]\n"
# "dir <name> <mode> <uid> <gid>\n"
# "nod <name> <mode> <uid> <gid> <dev_type> <maj> <min>\n"
# "slink <name> <target> <mode> <uid> <gid>\n"
# "pipe <name> <mode> <uid> <gid>\n"
# "sock <name> <mode> <uid> <gid>\n"
#
# For the location of a file, environment variables may be used, i.e.: ${MY_INIT}
# The mode is expected to be in octal.
#
# Empty lines are skipped over. Lines starting with '#' constitute comments and are
# ignored unless they provide a configuration condition. The grammar
# for the condition is:
#
# ```
# cfg_cond
#       : "#[cfg(" cond ")]"
#       ;
#
# cond :
#      any_list
#      | all_list
#      | expr
#      ;
#
# any_list :
#      "any(" expr_list ")"
#      ;
#
# all_list :
#      "all(" expr_list ")"
#      ;
#
# expr_list :
#      expr
#      | expr "," expr_list
#      ;
#
# expr :
#      term "==" term
#      | term "!=" term
#      | cond
#      ;
#
# term :
#      | '"' TOK_STRING '"'
#      ; "$" TOK_IDENT
#
# ```
#
# That is a context-free grammar (the proof is left to the reader). It has
# no empty productions, no left recursion so allows for the easy LL(1) parsing
# with no external dependencies/tooling req'd as opposed to the LR parsing.
#
# On the semantic side, if several `cfg` directives are used in a row, only
# if the last one matters (the other choice might provide a semblance of the
# conditional operator yet appears to make reasoning harder).

import io
import operator
import os
import stat
import time
import warnings

from typing import BinaryIO, List


class CpioEntry(object):
    def __init__(self, inode: int, name: str, mode: int, uid: int,
                gid: int, nlink: int, mtime: int, major: int, minor: int, rmajor: int, rminor: int,
                chksum: int, content: BinaryIO) -> None:
        name = os.path.expandvars(name)
        if len(name) > 255:
            raise Exception(f"The entry name '{name}' is too long")

        if name.startswith('/'):
            name = name[1:]

        filesize = content.seek(0, io.SEEK_END);

        self.name = name
        self.inode = inode
        self.mode = int(mode)
        self.uid = int(uid)
        self.gid = int(gid)
        self.nlink = int(nlink)
        self.mtime = int(mtime)
        self.filesize = filesize
        self.major = int(major)
        self.minor = int(minor)
        self.rmajor = int(rmajor)
        self.rminor = int(rminor)
        self.namesize = len(name)+1
        self.chksum = int(chksum)
        self.content = content

    def __repr__(self) -> str:
        return None

    def write(self, buffer: BinaryIO) -> None:
        def align_on_dword(buffer):
            while buffer.tell() & 3 != 0:
                buffer.write(b'\x00')

        name_bytes = bytearray(self.name, 'ascii')
        name_bytes.append(0)

        if self.filesize > 0 and self.nlink > 1:
            # Create entries for hardlinks. They all share the same inode

            header_bytes = bytes(
                f'070701{self.inode:08X}{self.mode:08X}{self.uid:08X}{self.gid:08X}{self.nlink:08X}{self.mtime:08X}' + \
                f'{0:08X}{self.major:08X}{self.minor:08X}{self.rmajor:08X}{self.rminor:08X}' + \
                f'{self.namesize:08X}{self.chksum:08X}',
                'ascii')

            assert(len(header_bytes) == 110)

            for i in range(self.nlink - 1):
                buffer.write(header_bytes)
                buffer.write(name_bytes)
                align_on_dword(buffer)

        # After the hard link entries are written (if any) write the file itself

        header_bytes = bytes(
            f'070701{self.inode:08X}{self.mode:08X}{self.uid:08X}{self.gid:08X}{self.nlink:08X}{self.mtime:08X}' + \
            f'{self.filesize:08X}{self.major:08X}{self.minor:08X}{self.rmajor:08X}{self.rminor:08X}' + \
            f'{self.namesize:08X}{self.chksum:08X}',
            'ascii')

        assert(len(header_bytes) == 110)

        buffer.write(header_bytes)
        buffer.write(name_bytes)
        align_on_dword(buffer)

        self.content.seek(0);
        while True:
            data = self.content.read(0x10000)
            if len(data) == 0:
                break
            buffer.write(data)

        align_on_dword(buffer)

class FileEntry(CpioEntry):
    def __init__(self, inode, name, location, mode, uid, gid, hard_links) -> None:
        location = os.path.expandvars(location)
        self.location = location
        self.hard_links = hard_links

        parameters = {
            'inode': inode,
            'name': name,
            'mode': int(mode) | stat.S_IFREG,
            'uid': uid,
            'gid': gid,
            'nlink': 1 + len(hard_links),
            'mtime': int(os.path.getmtime(location)),
            'major': 3,
            'minor': 1,
            'rmajor': 0,
            'rminor': 0,
            'chksum': 0,
            'content': open(location, 'rb')
        }

        super(FileEntry, self).__init__(**parameters)

    def __repr__(self) -> str:
        return f"file {self.name} {self.location} {self.mode:06o} {self.uid} {self.gid} {' '.join(self.hard_links)}"


class DirEntry(CpioEntry):
    def __init__(self, inode, name, mode, uid, gid) -> None:
        parameters = {
            'inode': inode,
            'name': name,
            'mode': int(mode) | stat.S_IFDIR,
            'uid': uid,
            'gid': gid,
            'nlink': 2,
            'mtime': int(time.time()),
            'major': 3,
            'minor': 1,
            'rmajor': 0,
            'rminor': 0,
            'chksum': 0,
            'content': io.BytesIO(b"")
        }

        super(DirEntry, self).__init__(**parameters)

    def __repr__(self) -> str:
        return f"dir {self.name} {self.mode:06o} {self.uid} {self.gid}"


class DeviceNodeEntry(CpioEntry):
    def __init__(self, inode, name, mode, uid, gid, dev_type, dev_maj, dev_min) -> None:
        if dev_type == 'c':
            mode = int(mode) | stat.S_IFCHR
        elif dev_type == 'b':
            mode = int(mode) | stat.S_IFBLK
        else:
            raise Exception("Invalid device type")

        parameters = {
            'inode': inode,
            'name': name,
            'mode': mode,
            'uid': uid,
            'gid': gid,
            'nlink': 1,
            'mtime': int(time.time()),
            'major': 3,
            'minor': 1,
            'rmajor': dev_maj,
            'rminor': dev_min,
            'chksum': 0,
            'content': io.BytesIO(b"")
        }

        super(DeviceNodeEntry, self).__init__(**parameters)

    def __repr__(self) -> str:
        return f"nod {self.name} {self.mode:06o} {self.uid} {self.gid} {self.rmajor} {self.rminor}"


class SymLinkEntry(CpioEntry):
    def __init__(self, inode, name, target, mode, uid, gid) -> None:
        content = bytearray(target, 'ascii')
        content.append(0)

        parameters = {
            'inode': inode,
            'name': name,
            'mode': int(mode) | stat.S_IFLNK,
            'uid': uid,
            'gid': gid,
            'nlink': 1,
            'mtime': int(time.time()),
            'major': 3,
            'minor': 1,
            'rmajor': 0,
            'rminor': 0,
            'chksum': 0,
            'content': io.BytesIO(content)
        }

        super(SymLinkEntry, self).__init__(**parameters)

    def __repr__(self) -> str:
        return f"slink {self.name} {self.target} {self.mode:06o} {self.uid} {self.gid}"


class PipeEntry(CpioEntry):
    def __init__(self, inode, name, mode, uid, gid) -> None:
        parameters = {
            'inode': inode,
            'name': name,
            'mode': int(mode) | stat.S_IFIFO,
            'uid': uid,
            'gid': gid,
            'nlink': 2,
            'mtime': int(time.time()),
            'major': 3,
            'minor': 1,
            'rmajor': 0,
            'rminor': 0,
            'chksum': 0,
            'content': io.BytesIO(b"")
        }

        super(PipeEntry, self).__init__(**parameters)

    def __repr__(self) -> str:
        return f"pipe {self.name} {self.mode:06o} {self.uid} {self.gid}"


class SocketEntry(CpioEntry):
    def __init__(self, inode, name, mode, uid, gid) -> None:
        parameters = {
            'inode': inode,
            'name': name,
            'mode': int(mode) | stat.S_IFSOCK,
            'uid': uid,
            'gid': gid,
            'nlink': 2,
            'mtime': int(time.time()),
            'major': 3,
            'minor': 1,
            'rmajor': 0,
            'rminor': 0,
            'chksum': 0,
            'content': io.BytesIO(b"")
        }

        super(SocketEntry, self).__init__(**parameters)

    def __repr__(self) -> str:
        return f"sock {self.name} {self.mode:06o} {self.uid} {self.gid}"


class TrailerEntry(CpioEntry):
    def __init__(self) -> None:
        parameters = {
            'inode': 0,
            'name': 'TRAILER!!!',
            'mode': 0,
            'uid': 0,
            'gid': 0,
            'nlink': 1,
            'mtime': 0,
            'major': 0,
            'minor': 0,
            'rmajor': 0,
            'rminor': 0,
            'chksum': 0,
            'content': io.BytesIO(b""),
        }

        super(TrailerEntry, self).__init__(**parameters)

    def __repr__(self) -> str:
        return f"{self.name}"


class CpioRamFs:
    def __init__(self, buffer_obj: BinaryIO):
        self.buffer_obj = buffer_obj
        self.opened = False

    def __enter__(self):
        self.opened = True
        return self

    def write(self, cpio_entry: CpioEntry):
        assert(self.opened)
        cpio_entry.write(self.buffer_obj)

    def __exit__(self, type, value, traceback):
        trailer = TrailerEntry()
        trailer.write(self.buffer_obj)

        while self.buffer_obj.tell() & 511 != 0:
            self.buffer_obj.write(b'\x00')

        self.buffer_obj.close()
        self.opened = False


class CfgCondEval:
    TOK_EOL = 0
    TOK_HASH = 1
    TOK_CFG = 2
    TOK_ANY = 3
    TOK_ALL = 4
    TOK_LPAREN = 5
    TOK_RPAREN = 6
    TOK_LBRAC = 7
    TOK_RBRAC = 8
    TOK_COMMA = 9
    TOK_ENV = 10
    TOK_EQ = 11
    TOK_NEQ = 12
    TOK_STR = 13

    TOKEN_REPR = [
        "<eol>",
        "#",
        "cfg",
        "any",
        "all",
        "(",
        ")",
        "[",
        "]",
        ",",
        "$",
        "==",
        "!=",
        "<string>"
    ]

    def __init__(self, cfg_cond_str, env):
        self.cfg_cond_str = cfg_cond_str
        self.token = CfgCondEval.TOK_EOL
        self.token_str = None
        self.env = env
        self.pos = 0

    def eval(self):
        self.lookahead()
        return self.cfg_cond()

    def lookahead(self):
        while self.pos < len(self.cfg_cond_str) and self.cfg_cond_str[self.pos] <= ' ':
            self.pos += 1

        if self.pos >= len(self.cfg_cond_str):
            self.token = CfgCondEval.TOK_EOL
            return

        head = self.cfg_cond_str[self.pos:]
        if head.startswith("#"):
            self.token = CfgCondEval.TOK_HASH
        elif head.startswith("cfg"):
            self.token = CfgCondEval.TOK_CFG
        elif head.startswith("any"):
            self.token = CfgCondEval.TOK_ANY
        elif head.startswith("all"):
            self.token = CfgCondEval.TOK_ALL
        elif head.startswith("("):
            self.token = CfgCondEval.TOK_LPAREN
        elif head.startswith(")"):
            self.token = CfgCondEval.TOK_RPAREN
        elif head.startswith("["):
            self.token = CfgCondEval.TOK_LBRAC
        elif head.startswith("]"):
            self.token = CfgCondEval.TOK_RBRAC
        elif head.startswith(","):
            self.token = CfgCondEval.TOK_COMMA
        elif head.startswith("$"):
            self.token = CfgCondEval.TOK_ENV
        elif head.startswith("=="):
            self.token = CfgCondEval.TOK_EQ
        elif head.startswith("!="):
            self.token = CfgCondEval.TOK_NEQ
        elif head.startswith('"'):
            self.token = CfgCondEval.TOK_STR
        else:
            raise Exception(f"no valid token found at the start of {head}")

        if self.token == CfgCondEval.TOK_ENV:
            self.pos += 1
            pos = self.pos
            while pos < len(self.cfg_cond_str) and (self.cfg_cond_str[pos].isalnum() or self.cfg_cond_str[pos] == "_"):
                pos += 1
            self.token_str = self.cfg_cond_str[self.pos:pos]
            self.pos = pos
        elif self.token == CfgCondEval.TOK_STR:
            self.pos += 1
            pos = self.pos
            while pos < len(self.cfg_cond_str) and self.cfg_cond_str[pos] != '"':
                pos += 1
            self.token_str = self.cfg_cond_str[self.pos:pos]
            self.pos = pos
            self.pos += 1
        else:
            self.token_str = CfgCondEval.TOKEN_REPR[self.token]
            self.pos += len(self.token_str)

        # print(f"lookahead: {self.token_str}")

    def consume(self, *expected_list):
        for expected in expected_list:
            if self.token != expected:
                raise SyntaxError(f"Expected '{CfgCondEval.TOKEN_REPR[self.token]}' but found '{self.token_str}' at pos. {self.pos}")
            self.lookahead()

    def cfg_cond(self):
        self.consume(CfgCondEval.TOK_HASH, CfgCondEval.TOK_LBRAC, CfgCondEval.TOK_CFG, CfgCondEval.TOK_LPAREN)
        result = self.cond()
        self.consume(CfgCondEval.TOK_RPAREN, CfgCondEval.TOK_RBRAC, CfgCondEval.TOK_EOL)
        # print(f"{self.cfg_cond_str} == {result}")
        return result

    def cond(self):
        if self.token == CfgCondEval.TOK_ANY:
            result = self.any_list()
        elif self.token == CfgCondEval.TOK_ALL:
            result = self.all_list()
        else:
            result = self.expr()
        return result

    def any_list(self):
        self.consume(CfgCondEval.TOK_ANY, CfgCondEval.TOK_LPAREN)
        exprs = self.expr_list()
        self.consume(CfgCondEval.TOK_RPAREN)
        return any(exprs)

    def all_list(self):
        self.consume(CfgCondEval.TOK_ALL, CfgCondEval.TOK_LPAREN)
        exprs = self.expr_list()
        self.consume(CfgCondEval.TOK_RPAREN)
        return all(exprs)

    def expr_list(self):
        exprs = [self.expr()]
        while self.token == CfgCondEval.TOK_COMMA:
            self.lookahead()
            exprs.append(self.expr())
        return exprs

    def expr(self):
        if self.token == CfgCondEval.TOK_ENV or self.token == CfgCondEval.TOK_STR:
            left = self.term()

            op = None
            if self.token == CfgCondEval.TOK_EQ:
                op = operator.eq
            elif self.token == CfgCondEval.TOK_NEQ:
                op = operator.ne
            if op == None:
                raise SyntaxError(f"Expected == or !=, position {self.pos}")
            self.lookahead()

            right = self.term()

            return op(left, right)
        else:
            return self.cond()

    def term(self):
        val = None
        if self.token == CfgCondEval.TOK_ENV:
            if self.token_str in self.env:
                val = self.env[self.token_str]
        elif self.token == CfgCondEval.TOK_STR:
            val = self.token_str
        else:
            raise SyntaxError(f"expected to find a string or an env. var. in {self.token_str}")
        self.lookahead()

        return val


class InitRamFsConfig:
    def __init__(self, config_files: List[str]) -> None:
        self.cpio_entries = []
        inode = 721

        for config_file in config_files:
            with open(config_file, 'rt') as f:
                cfg_cond = False
                skip_next_line = False
                for line_idx, line in enumerate(f):
                    line = line.strip()
                    if not line:
                        continue

                    if line.startswith("#[cfg(") and line.endswith(")]"):
                        if cfg_cond:
                            raise UserWarning(f"previous cfg() ignored, line {line_idx+1}")
                        cfg_cond = True
                        cfg_cond_eval = CfgCondEval(line, os.environ)
                        skip_next_line = not cfg_cond_eval.eval()
                        continue

                    if line.startswith('#'):
                        continue

                    if cfg_cond:
                        cfg_cond = False

                    if skip_next_line:
                        skip_next_line = False
                        continue

                    parts = line.split()
                    if len(parts) == 0:
                        continue

                    cpio_entry = None

                    try:
                        if parts[0] == "file":
                            name, location, mode, uid, gid, *hard_links = parts[1:]
                            mode = int(mode, 8)
                            cpio_entry = FileEntry(inode, name, location, mode, uid, gid, hard_links)
                        elif parts[0] == "dir":
                            name, mode, uid, gid = parts[1:]
                            mode = int(mode, 8)
                            cpio_entry = DirEntry(inode, name, mode, uid, gid)
                        elif parts[0] == "nod":
                            name, mode, uid, gid, dev_type, dev_maj, dev_min = parts[1:]
                            mode = int(mode, 8)
                            cpio_entry = DeviceNodeEntry(inode, name, mode, uid, gid, dev_type, dev_maj, dev_min)
                        elif parts[0] == "slink":
                            name, target, mode, uid, gid = parts[1:]
                            mode = int(mode, 8)
                            cpio_entry = SymLinkEntry(inode, name, target, mode, uid, gid)
                        elif parts[0] == "pipe":
                            name, mode, uid, gid = parts[1:]
                            mode = int(mode, 8)
                            cpio_entry = PipeEntry(inode, name, mode, uid, gid)
                        elif parts[0] == "sock":
                            name, mode, uid, gid = parts[1:]
                            mode = int(mode, 8)
                            cpio_entry = SocketEntry(inode, name, mode, uid, gid)
                        else:
                            raise Exception(f"Can't parse: {line}")
                    except ValueError:
                        raise Exception(f"Can't parse: {line}")

                    self.cpio_entries.append(cpio_entry)
                    inode += 1
                    # print(cpio_entry)

    def entries(self):
        return self.cpio_entries


def __open_output_stream(file_name: str, compression: str):
    mode = 'xb'
    if compression == 'none':
        return open(file_name, mode)
    elif compression == 'bz2':
        import bz2
        return bz2.open(file_name, mode)
    elif compression == 'gzip':
        import gzip
        return gzip.open(file_name, mode, compresslevel=6)
    elif compression == 'lzma':
        import lzma
        return lzma.open(file_name, mode)
    else:
        raise Exception("Unknown compression algorithm")


def create_cpio_from_config(config_files: List[str], output_file: str, compression: str):
    with __open_output_stream(output_file, compression) as ostream:
        with CpioRamFs(ostream) as cpio:
            config = InitRamFsConfig(config_files)
            for entry in config.entries():
                cpio.write(entry)


def create_cpio_from_dir(top_dir: str, output_file: str, compression: str):
    if not os.path.isdir(top_dir):
        raise Exception(f"{top_dir} is not a directory")
    with __open_output_stream(output_file, compression) as ostream:
        with CpioRamFs(ostream) as cpio:
            for root, dirs, files in os.walk(top_dir):
                # Notes:
                # - No support for symlinks currently
                # - file/dir mode is copied from the source
                # - uid/gid are set to 0/0
                for dir in dirs:
                    location = os.path.join(root, dir)
                    if os.path.islink(location):
                        warnings.warn(f"Symlink {location} is skipped, not suppoorted")
                        continue
                    name = location.replace(top_dir, "/")
                    stat_info = os.stat(location)
                    dir_entry = DirEntry(name, mode=stat_info.st_mode, uid=0, gid=0)
                    # print(dir_entry)
                    cpio.write(dir_entry)
                for file in files:
                    location = os.path.join(root, file)
                    name = location.replace(top_dir, "/")
                    stat_info = os.stat(location)
                    file_entry = FileEntry(name, location, mode=stat_info.st_mode, uid=0, gid=0, hard_links=[])
                    # print(file_entry)
                    cpio.write(file_entry)

if __name__ == '__main__':
    import argparse

    parser = argparse.ArgumentParser()

    parser.add_argument('config_file_or_dir', help='Initial RAM FS configuration file or the top directory')
    parser.add_argument('output_file', help='Output file that contains the initial RAM FS')
    parser.add_argument('--compression', required=False, help='Compression to use, default is gzip',
        choices=('gzip', 'bz2', 'lzma', 'none'), default='gzip')

    args = parser.parse_args()

    if not os.path.isdir(args.config_file_or_dir):
        create_cpio_from_config([args.config_file_or_dir], args.output_file, args.compression)
    else:
        create_cpio_from_dir(args.config_file_or_dir, args.output_file, args.compression)

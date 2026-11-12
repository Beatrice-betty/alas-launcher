"""构建 Windows 独立包的应用文件和可更新的签名网页包。"""

import argparse
import base64
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import zipfile
from pathlib import Path

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey


def make_key(path):
    if not path.is_file():
        path.parent.mkdir(parents=True, exist_ok=True)
        key = Ed25519PrivateKey.generate()
        fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        with os.fdopen(fd, 'wb') as file:
            file.write(key.private_bytes_raw())
    return Ed25519PrivateKey.from_private_bytes(path.read_bytes())


def write_deploy(root):
    template = root / 'config/deploy.template.yaml'
    data = template.read_text(encoding='utf-8')
    values = {
        'Repository': 'https://gitcode.com/Beatrice-betty/AzurPilot',
        'Branch': 'dev',
        'GitExecutable': './runtime/git/cmd/git.exe',
        'PythonExecutable': './runtime/python/python.exe',
        'AdbExecutable': './runtime/adb/adb.exe',
        'InstallDependencies': 'false',
        'ReplaceAdb': 'false',
        'EnableReload': 'true',
        'WebuiHost': '127.0.0.1',
        'WebuiPort': '25549',
        'AutoRestartTime': 'null',
    }
    for name, value in values.items():
        data, count = re.subn(rf'(?m)^(\s*{name}:)[^\n]*$', rf'\1 {value}', data)
        if count != 1:
            raise ValueError(f'部署模板字段异常 {name}')
    (root / 'config/deploy.yaml').write_text(data, encoding='utf-8')


def copytree(source, destination):
    if not source.is_dir():
        raise FileNotFoundError(source)
    shutil.copytree(source, destination, dirs_exist_ok=True, ignore=shutil.ignore_patterns('__pycache__', '*.pyc'))


def assemble(alas, release, key_path, *, package=None, inputs=None, python_root=None, launcher=None):
    sys.path.insert(0, str(alas))
    from deploy import independent

    git_status = subprocess.check_output(['git', 'status', '--porcelain', '--untracked-files=no'], cwd=alas)
    if git_status:
        raise ValueError('请先提交将要打包的 ALAS 变更')
    revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=alas, text=True).strip()
    dist = alas / 'frontend/dist'
    if not (dist / 'index.html').is_file():
        raise FileNotFoundError(dist / 'index.html')
    if 'clarity.ms' in (dist / 'index.html').read_text(encoding='utf-8'):
        raise ValueError('构建的 WebUI 仍含 Clarity')
    key = make_key(key_path)
    release.mkdir(parents=True, exist_ok=False)
    frontend = release / 'frontend.zip'
    with zipfile.ZipFile(frontend, 'w', compression=zipfile.ZIP_DEFLATED, compresslevel=5) as archive:
        for path in sorted(dist.rglob('*')):
            if path.is_file():
                archive.write(path, 'frontend/dist/' + path.relative_to(dist).as_posix())

    with tempfile.TemporaryDirectory(prefix='azurpilot-application-', dir=release.parent) as temporary:
        temporary = Path(temporary)
        export = temporary / 'source.zip'
        subprocess.run(['git', 'archive', '--format=zip', f'--output={export}', revision], cwd=alas, check=True)
        root = package or temporary / 'app'
        root.mkdir(parents=True, exist_ok=False)
        files = independent.unpack(export, root)
        files.update(independent.unpack(frontend, root, frontend=True))
        data = {
            'schema': 1,
            'revision': revision,
            'sequence': time.time_ns() // 1_000_000,
            'dependencies': independent.dependency_id(root),
            'frontendSha256': independent.digest(frontend),
        }
        independent.write_json(release / 'release.json', {
            'release': data,
            'signature': base64.b64encode(key.sign(independent.canonical(data))).decode(),
        })
        if package is None:
            return
        independent.write_json(root / independent.RECEIPT, {
            **data, 'publicKey': base64.b64encode(key.public_key().public_bytes_raw()).decode(),
        })
        independent.write_json(root / independent.FILES, files)
        write_deploy(root)
        runtime = root / 'runtime'
        copytree(python_root, runtime / 'python')
        copytree(inputs / 'python-build/Lib/site-packages', runtime / 'python/Lib/site-packages')
        copytree(inputs / 'mingit', runtime / 'git')
        copytree(inputs / 'adb/platform-tools', runtime / 'adb')
        copytree(inputs / 'atx', runtime / 'atx')
        webview_exe, = (inputs / 'webview2').glob('*/msedgewebview2.exe')
        copytree(webview_exe.parent, runtime / 'webview2')
        shutil.copy2(root / 'deploy/independent.py', runtime / 'independent_update.py')
        shutil.copy2(launcher, root / 'AzurPilot.exe')
        shutil.copy2(Path(__file__).resolve().parents[1] / 'LICENSE', root / 'launcher-LICENSE.txt')
        independent.write_json(runtime / 'components.json', {
            'python': subprocess.check_output([str(runtime / 'python/python.exe'), '--version'], text=True).strip(),
            'git': subprocess.check_output([str(runtime / 'git/cmd/git.exe'), '--version'], text=True).strip(),
            'webview2': webview_exe.parent.name,
            'adb': subprocess.check_output([str(runtime / 'adb/adb.exe'), 'version'], text=True).splitlines()[1],
        })
        (root / '使用说明.txt').write_text(
            '打开 AzurPilot.exe 即可启动 ALAS。控制台默认使用本机端口 25549。\n'
            '控制台的更新页只更新 ALAS 程序和网页。运行环境由完整安装包更新。\n'
            '旧版配置和历史请先备份，再迁移到新安装目录。\n'
            'ALAS 与启动器的来源、许可证见软件附带文件。\n',
            encoding='utf-8',
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--alas', required=True, type=Path)
    parser.add_argument('--release', required=True, type=Path)
    parser.add_argument('--key', required=True, type=Path)
    parser.add_argument('--package', type=Path)
    parser.add_argument('--inputs', type=Path)
    parser.add_argument('--python-root', type=Path)
    parser.add_argument('--launcher', type=Path)
    args = parser.parse_args()
    if args.package and not all((args.inputs, args.python_root, args.launcher)):
        parser.error('--package 需要 --inputs、--python-root 和 --launcher')
    assemble(
        args.alas.resolve(), args.release.resolve(), args.key.resolve(),
        package=args.package.resolve() if args.package else None,
        inputs=args.inputs, python_root=args.python_root, launcher=args.launcher,
    )


if __name__ == '__main__':
    main()

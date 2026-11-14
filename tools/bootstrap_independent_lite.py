"""Download and stage the Windows runtime from the launcher."""

import argparse
import hashlib
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import traceback
import zipfile
from pathlib import Path

BOOTSTRAP = Path(__file__).resolve().parent
CREATE_NO_WINDOW = getattr(subprocess, 'CREATE_NO_WINDOW', 0)


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def download(spec, directory, *, refresh=False):
    destination = directory / spec['filename']
    if destination.resolve() != destination:
        raise ValueError(f'下载文件路径无效 {destination.name}')
    if not refresh and destination.is_file() and digest(destination) == spec['sha256']:
        return destination

    curl = Path(os.environ['SystemRoot']) / 'System32/curl.exe'
    failures = []
    for url in spec['urls']:
        with tempfile.NamedTemporaryFile(dir=directory, delete=False) as stream:
            temporary = Path(stream.name)
        try:
            print(f'Download {destination.name} from {url}', flush=True)
            result = subprocess.run([
                str(curl), '-q', '--fail', '--silent', '--show-error', '--location',
                '--proto', '=https', '--proto-redir', '=https', '--connect-timeout', '15',
                '--speed-limit', '1024', '--speed-time', '35', '--max-time', '600',
                '--output', str(temporary), '--', url,
            ], check=False, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
                creationflags=CREATE_NO_WINDOW, timeout=610)
            if result.returncode:
                raise OSError(result.stderr.decode('utf-8', errors='replace').strip())
            if digest(temporary) != spec['sha256']:
                raise ValueError('下载内容校验失败')
            temporary.replace(destination)
            return destination
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            failures.append(str(error))
            print(f'{destination.name}: {error}', flush=True)
        finally:
            temporary.unlink(missing_ok=True)
    raise RuntimeError(f'{destination.name} 下载失败：' + ' / '.join(failures))


def fetch_components(bundle, cache, wheels, repair):
    from concurrent.futures import ThreadPoolExecutor, as_completed

    tools = ('git', 'adb', 'nodejs')
    for component in tools:
        print(f'COMPONENT {bundle[component]["filename"]}', flush=True)
        download(bundle[component], cache, refresh=repair)
    wheels.mkdir(exist_ok=True)
    if wheels.resolve() != wheels:
        raise ValueError('依赖缓存不能指向其他位置。')
    for spec in bundle['wheels']:
        if 'embedded' in spec:
            source = BOOTSTRAP / 'wheels' / spec['filename']
            if digest(source) != spec['sha256']:
                raise ValueError(f'内置依赖校验失败 {source.name}')
            destination = wheels / source.name
            if destination.resolve() != destination:
                raise ValueError(f'依赖缓存文件路径无效 {source.name}')
            shutil.copy2(source, destination)
    jobs = [spec for spec in bundle['wheels'] if 'embedded' not in spec]
    executor = ThreadPoolExecutor(max_workers=4)
    try:
        pending = [executor.submit(download, spec, wheels) for spec in jobs]
        for complete, future in enumerate(as_completed(pending), 1):
            future.result()
            print(f'DOWNLOADED {complete}/{len(jobs)}', flush=True)
    finally:
        executor.shutdown(cancel_futures=True)


def run(command, *, cwd, timeout):
    env = {
        name: value for name, value in os.environ.items()
        if not name.upper().startswith(('PIP_', 'PYTHON', 'UV_')) and name.upper() != 'VIRTUAL_ENV'
    }
    env.update(PIP_CONFIG_FILE=os.devnull, PYTHONNOUSERSITE='1')
    result = subprocess.run(
        [str(part) for part in command], cwd=cwd, env=env,
        check=False, timeout=timeout, capture_output=True, creationflags=CREATE_NO_WINDOW,
    )
    if result.returncode:
        detail = (result.stderr or result.stdout).decode('utf-8', errors='replace').strip()
        raise RuntimeError(f'{Path(command[0]).name} 执行失败（{result.returncode}）\n{detail[-3000:]}')


def ensure_vc_runtime(bundle, cache):
    import winreg

    key_name = r'SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64'
    for view in (winreg.KEY_WOW64_64KEY, winreg.KEY_WOW64_32KEY):
        try:
            with winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, key_name, 0, winreg.KEY_READ | view) as key:
                installed = winreg.QueryValueEx(key, 'Installed')[0]
                version = winreg.QueryValueEx(key, 'Version')[0]
            if (installed == 1 and re.fullmatch(r'v?\d+\.\d+\.\d+\.\d+', version, re.IGNORECASE)
                    and tuple(map(int, version.lstrip('vV').split('.'))) >= (14, 51, 36247, 0)):
                return
        except FileNotFoundError:
            continue
    spec = bundle['vc_runtime']
    print(f'COMPONENT {spec["filename"]}', flush=True)
    installer = download(spec, cache)
    # ShellExecute prompts for Windows elevation only when the prerequisite is missing.
    powershell = Path(os.environ['SystemRoot']) / 'System32/WindowsPowerShell/v1.0/powershell.exe'
    env = dict(os.environ, AZURPILOT_VC_SETUP=str(installer))
    result = subprocess.run(
        [str(powershell), '-NoProfile', '-NonInteractive', '-Command',
         ("$ErrorActionPreference='Stop'; $p=Start-Process -FilePath $env:AZURPILOT_VC_SETUP "
          "-ArgumentList '/install','/quiet','/norestart' -Verb RunAs -Wait -PassThru; exit $p.ExitCode")],
        check=False, timeout=300, env=env, creationflags=CREATE_NO_WINDOW,
    )
    if result.returncode in (3010, 1641):
        raise RuntimeError('请重启 Windows，重新打开 AzurPilot 后继续部署。')
    result.check_returncode()


def install_runtime(bundle, cache, wheels, runtime):
    python = runtime / 'python'
    shutil.copytree(BOOTSTRAP / 'python', python)
    for name in ('git', 'adb'):
        target = runtime if name == 'adb' else runtime / name
        with zipfile.ZipFile(cache / bundle[name]['filename']) as archive:
            archive.extractall(target)
    (runtime / 'platform-tools').replace(runtime / 'adb')
    shutil.copytree(BOOTSTRAP / 'atx', runtime / 'atx')
    node = bundle['nodejs']['filename']
    with zipfile.ZipFile(cache / node) as archive:
        archive.extractall(runtime)
    (runtime / Path(node).stem).rename(runtime / 'nodejs')

    print('STEP 4', flush=True)
    exe = python / 'python.exe'
    site_packages = python / 'Lib/site-packages'
    uv = bundle['uv']
    uv_wheel = wheels / uv['filename']
    if digest(uv_wheel) != uv['sha256']:
        raise ValueError('安装工具校验失败。')
    # 使用锁文件中的 uv 安装原版本的依赖，包括带旧元数据的 wheel。
    tool = runtime.parent.parent / 'uv.exe'
    with zipfile.ZipFile(uv_wheel) as archive:
        member, = (name for name in archive.namelist() if name.endswith('.data/scripts/uv.exe'))
        with archive.open(member) as source, tool.open('wb') as destination:
            shutil.copyfileobj(source, destination)
    run([
        tool, '--no-config', '--offline', '--no-cache', 'pip', 'install',
        '--no-index', '--no-deps', '--no-build', '--require-hashes', '--quiet',
        '--target', site_packages, '--python', exe, '--find-links', wheels,
        '-r', BOOTSTRAP / 'requirements.txt',
    ], cwd=BOOTSTRAP, timeout=600)
    shutil.copy2(BOOTSTRAP / 'components.json', runtime)
    return site_packages


def load_updater(site_packages):
    sys.path.insert(0, str(BOOTSTRAP))
    sys.path.insert(0, str(site_packages))
    spec = importlib.util.spec_from_file_location('independent', BOOTSTRAP / 'independent.py')
    updater = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(updater)
    return updater


def stage_application(updater, root, stage, installed):
    receipt = updater.read_json(root / updater.RECEIPT) if installed else updater.read_json(BOOTSTRAP / 'receipt.json')
    missing = {}
    if installed:
        for name, expected in updater.read_json(root / updater.FILES).items():
            target = updater.check_target(root, name)
            if not target.is_file() or not target.stat().st_size:
                missing[name] = expected
        if not missing:
            return
    print('STEP 5', flush=True)
    with tempfile.TemporaryDirectory(prefix='alas-', dir=stage.parent, ignore_cleanup_errors=True) as directory:
        directory = Path(directory)
        if installed:
            # Repair only missing or damaged managed files from the installed revision.
            revision = receipt['revision']
            repo, branch = updater.update_source(root)
            offer = updater.GitOffer({'revision': revision}, repo, branch)
        else:
            # Query the configured branch even if the installer was downloaded earlier.
            config = stage / 'config/deploy.yaml'
            config.parent.mkdir()
            repo, branch = updater.update_source(root)
            config.write_text(f'Repository: "{repo}"\nBranch: "{branch}"\n', encoding='utf-8')
            updater.write_json(stage / updater.RECEIPT, {'revision': None})
            offer = updater.check(stage)
            config.unlink()
            revision = offer.release['revision']
        print(f'ALAS source {offer.repository} branch {offer.branch} revision {revision}', flush=True)
        try:
            source, checkout = updater._fetch_source(stage, directory, offer)
        except RuntimeError as error:
            cause = error.__cause__
            details = getattr(cause, 'stderr', None)
            if details:
                print(details.decode('utf-8', errors='replace').strip(), flush=True)
            raise
        archive = directory / 'source.zip'
        updater.git(stage, checkout, 'archive', '--format=zip', f'--output={archive}', revision)
        print(f'SOURCE {source} {revision}', flush=True)
        candidate = directory / 'application'
        candidate.mkdir()
        contents = updater.unpack(archive, candidate)
        if updater.dependency_id(candidate) != receipt['dependencies']:
            raise ValueError('此 ALAS 版本需要不同的 Python 依赖，请使用匹配的新版安装包。')
        contents.update(updater._frontend_for_commit(stage, source, revision, archive, directory, candidate))
        if not (candidate / 'frontend/dist/index.html').is_file():
            raise ValueError('ALAS 缺少 WebUI 文件。')
        print('STEP 6', flush=True)
        updater.probe(stage, candidate)
        files = missing if installed else contents
        for name in files:
            if installed and missing[name] != contents.get(name):
                raise ValueError(f'当前提交无法恢复 {name}，请使用对应版本的完整包。')
            destination = stage / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            (candidate / name).replace(destination)

    if not installed:
        updater.write_json(stage / updater.RECEIPT, {**receipt, 'revision': revision,
                           'frontendSha256': None})
        updater.write_json(stage / updater.FILES, contents)
    update_cache = stage / updater.WORK
    if update_cache.is_dir():
        # Git writes read-only objects on Windows. The launcher cleans the
        # transaction directory, including read-only files, after committing.
        update_cache.rename(stage.parent / 'update-cache')


def prepare(root, stage, repair):
    root = root.resolve()
    stage = stage.resolve()
    cache = root / '.azurpilot/lite-setup'
    if cache.resolve() != cache:
        raise ValueError('下载缓存不能指向其他位置。')
    cache.mkdir(parents=True, exist_ok=True)
    bundle = json.loads((BOOTSTRAP / 'downloads.json').read_text(encoding='utf-8'))
    installed = (root / '.azurpilot/independent.json').is_file()
    if (root / '.azurpilot/independent-update/transaction.json').is_file():
        raise RuntimeError('请先启动一次恢复未完成的 ALAS 更新，再修复运行环境。')
    if not installed and any((root / name).exists() for name in ('gui.py', '.git', 'runtime/python/python.exe')):
        raise RuntimeError('当前目录已有其他版本的程序，请选择独立的安装目录。')
    if installed:
        old = json.loads((root / '.azurpilot/independent.json').read_text(encoding='utf-8'))
        packaged = json.loads((BOOTSTRAP / 'receipt.json').read_text(encoding='utf-8'))
        if old['dependencies'] != packaged['dependencies']:
            raise ValueError('已安装版本使用不同的依赖，请用匹配该版本的安装包修复。')
    ensure_vc_runtime(bundle, cache)
    wheels = cache / 'wheels'
    fetch_components(bundle, cache, wheels, repair)
    site_packages = install_runtime(bundle, cache, wheels, stage / 'runtime')
    updater = load_updater(site_packages)
    stage_application(updater, root, stage, installed)
    shutil.copy2(BOOTSTRAP / 'independent.py', stage / 'runtime/independent_update.py')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('root', type=Path)
    parser.add_argument('stage', type=Path)
    parser.add_argument('--repair', action='store_true')
    args = parser.parse_args()
    try:
        prepare(args.root, args.stage, args.repair)
    except Exception as error:  # noqa: BLE001 - surface the concise error and keep its traceback in the launcher log
        traceback.print_exc()
        raise SystemExit(str(error)) from None


if __name__ == '__main__':
    main()

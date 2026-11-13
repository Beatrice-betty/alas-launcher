"""Prepare the pinned Windows runtime and ALAS for the light installer."""

import argparse
import concurrent.futures
import hashlib
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path


BOOTSTRAP = Path(__file__).resolve().parent
CACHE = Path('.azurpilot/lite-setup')
CREATE_NO_WINDOW = getattr(subprocess, 'CREATE_NO_WINDOW', 0)


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def cache_directory(root):
    path = root / CACHE
    if root.resolve() != root or path.resolve() != path:
        raise ValueError('安装或下载目录不能指向其他位置。')
    return path


def download(spec, directory):
    destination = directory / spec['filename']
    if destination.resolve() != destination:
        raise ValueError(f'下载文件路径无效 {destination.name}')
    if destination.is_file() and digest(destination) == spec['sha256']:
        return destination

    curl = Path(os.environ['SystemRoot']) / 'System32/curl.exe'
    failures = []
    for _ in range(3):
        for url in spec['urls']:
            with tempfile.NamedTemporaryFile(dir=directory, delete=False) as stream:
                temporary = Path(stream.name)
            try:
                result = subprocess.run([
                    str(curl), '-q', '--fail', '--silent', '--show-error', '--location',
                    '--proto', '=https', '--proto-redir', '=https', '--connect-timeout', '15',
                    '--speed-limit', '1024', '--speed-time', '35', '--max-time', '900',
                    '--output', str(temporary), '--', url,
                ], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, creationflags=CREATE_NO_WINDOW, timeout=960)
                if result.returncode:
                    raise OSError(result.stderr.decode('utf-8', errors='replace').strip())
                if digest(temporary) != spec['sha256']:
                    raise ValueError('下载内容校验失败')
                temporary.replace(destination)
                return destination
            except (OSError, ValueError, subprocess.SubprocessError) as error:
                failures.append(str(error))
            finally:
                temporary.unlink(missing_ok=True)
    raise RuntimeError(f'{destination.name} 下载失败，' + ' / '.join(dict.fromkeys(failures)))


def wheel_cache(bundle, cache):
    directory = cache / 'wheels'
    directory.mkdir(exist_ok=True)
    if directory.resolve() != directory:
        raise ValueError('依赖缓存不能指向其他位置。')
    for spec in bundle['wheels']:
        if 'embedded' in spec:
            wheel = BOOTSTRAP / 'wheels' / spec['filename']
            if digest(wheel) != spec['sha256']:
                raise ValueError(f'内置依赖校验失败 {wheel.name}')
            shutil.copy2(wheel, directory / wheel.name)
    return directory


def fetch_components(bundle, cache, wheels):
    jobs = [(spec, wheels) for spec in bundle['wheels'] if 'embedded' not in spec]
    jobs.insert(0, (bundle['webview2'], cache))
    if 'nodejs' in bundle:
        jobs.insert(1, (bundle['nodejs'], cache))
    print('STEP 2', flush=True)
    failures = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
        pending = [executor.submit(download, spec, directory) for spec, directory in jobs]
        for complete, future in enumerate(concurrent.futures.as_completed(pending), 1):
            try:
                future.result()
            except RuntimeError as error:
                failures.append(str(error))
            else:
                print(f'DOWNLOADED {complete - len(failures)}/{len(pending)}', flush=True)
    if failures:
        raise RuntimeError('以下文件下载失败，请重试：\n' + '\n'.join(failures))


def run(command, *, cwd, timeout):
    env = {
        name: value for name, value in os.environ.items()
        if not name.upper().startswith(('PIP_', 'PYTHON', 'UV_')) and name.upper() != 'VIRTUAL_ENV'
    }
    env.update(PIP_CONFIG_FILE=os.devnull, PYTHONNOUSERSITE='1')
    result = subprocess.run(
        [str(part) for part in command], cwd=cwd, env=env,
        timeout=timeout, capture_output=True, creationflags=CREATE_NO_WINDOW,
    )
    if result.returncode:
        detail = (result.stderr or result.stdout).decode('utf-8', errors='replace').strip()
        raise RuntimeError(f'{Path(command[0]).name} 执行失败（{result.returncode}）\n{detail[-3000:]}')


def install_vc(path, sha256):
    trusted = BOOTSTRAP / path.name
    shutil.copy2(path, trusted)
    if digest(trusted) != sha256:
        raise ValueError('运行库校验失败。')
    result = subprocess.run(
        [str(trusted), '/install', '/quiet', '/norestart'],
        timeout=300, creationflags=CREATE_NO_WINDOW,
    )
    if result.returncode in (3010, 1641):
        raise SystemExit(3010)
    result.check_returncode()


def install_runtime(bundle, cache, wheels, stage):
    runtime = stage / 'runtime'
    python = runtime / 'python'
    shutil.copytree(BOOTSTRAP / 'python', python)
    for name in ('git', 'adb'):
        target = runtime if name == 'adb' else runtime / name
        with zipfile.ZipFile(BOOTSTRAP / f'{name}.zip') as archive:
            archive.extractall(target)
    (runtime / 'platform-tools').replace(runtime / 'adb')
    shutil.copytree(BOOTSTRAP / 'atx', runtime / 'atx')
    if 'nodejs' in bundle:
        node = bundle['nodejs']
        path = cache / node['filename']
        if digest(path) != node['sha256']:
            raise ValueError('Node.js 下载文件校验失败。')
        with zipfile.ZipFile(path) as archive:
            with tempfile.TemporaryDirectory(prefix='node-', dir=BOOTSTRAP) as directory:
                archive.extractall(directory)
                shutil.move(Path(directory) / Path(node['filename']).stem, runtime / 'nodejs')

    print('STEP 3', flush=True)
    with tempfile.TemporaryDirectory(prefix='webview2-', dir=BOOTSTRAP) as unpack:
        cab = cache / bundle['webview2']['filename']
        expand = Path(os.environ['SystemRoot']) / 'System32/expand.exe'
        run([expand, str(cab), '-F:*', unpack], cwd=BOOTSTRAP, timeout=300)
        browser, = Path(unpack).rglob('msedgewebview2.exe')
        shutil.move(browser.parent, runtime / 'webview2')

    print('STEP 4', flush=True)
    exe = python / 'python.exe'
    site_packages = python / 'Lib/site-packages'
    uv = bundle['uv']
    uv_wheel = wheels / uv['filename']
    if digest(uv_wheel) != uv['sha256']:
        raise ValueError('安装工具校验失败。')
    # 使用锁文件中的 uv 安装原版本的依赖，包括带旧元数据的 wheel。
    tool = BOOTSTRAP / 'uv.exe'
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
    sys.path.insert(0, str(site_packages))
    spec = importlib.util.spec_from_file_location('independent', BOOTSTRAP / 'independent.py')
    updater = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(updater)
    return updater


def fetch_application(updater, revision, stage, directory):
    failures = []
    for repository in updater.SOURCES:
        try:
            checkout = Path(tempfile.mkdtemp(prefix='git-', dir=directory))
            updater.git(stage, checkout, 'init', '-q')
            updater.git(stage, checkout, 'fetch', '--depth=1', '--no-tags', repository, revision, timeout=300)
            archive = directory / 'source.zip'
            updater.git(stage, checkout, 'archive', '--format=zip', f'--output={archive}', revision)
            print(f'SOURCE {repository}', flush=True)
            return archive
        except (OSError, subprocess.SubprocessError) as error:
            failures.append(f'{repository} {error}')
    raise RuntimeError('ALAS 源码下载失败，' + ' / '.join(failures))


def install_application(updater, stage):
    receipt = updater.read_json(BOOTSTRAP / 'receipt.json')
    release = updater.verify_manifest(updater.read_json(BOOTSTRAP / 'release.json'), receipt['publicKey'])
    frontend = BOOTSTRAP / 'frontend.zip'
    if any(receipt.get(name) != value for name, value in release.items()):
        raise ValueError('包内发布清单与安装版本不匹配。')
    if updater.digest(frontend) != release['frontendSha256']:
        raise ValueError('包内网页校验失败。')

    print('STEP 5', flush=True)
    with tempfile.TemporaryDirectory(prefix='alas-', dir=BOOTSTRAP) as directory:
        directory = Path(directory)
        source = fetch_application(updater, release['revision'], stage, directory)
        candidate = directory / 'application'
        candidate.mkdir()
        files = updater.unpack(source, candidate)
        files.update(updater.unpack(frontend, candidate, frontend=True))
        if updater.dependency_id(candidate) != release['dependencies']:
            raise ValueError('ALAS 提交的依赖清单不匹配。')
        print('STEP 6', flush=True)
        updater.probe(stage, candidate)
        for name in files:
            destination = stage / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            (candidate / name).replace(destination)

    updater.write_json(stage / updater.RECEIPT, receipt)
    updater.write_json(stage / updater.FILES, files)
    shutil.copy2(stage / 'deploy/independent.py', stage / 'runtime/independent_update.py')
    shutil.copy2(BOOTSTRAP / 'deploy.yaml', stage / 'config/deploy.yaml')


def prepare(root, need_vc, *, vc_only=False):
    cache = cache_directory(root)
    cache.mkdir(parents=True, exist_ok=True)
    try:
        bundle = json.loads((BOOTSTRAP / 'downloads.json').read_text(encoding='utf-8'))
        if need_vc:
            print('STEP VC', flush=True)
            vc = bundle['vc_runtime']
            install_vc(download(vc, cache), vc['sha256'])
        if vc_only:
            (cache / 'error.txt').unlink(missing_ok=True)
            (cache / vc['filename']).unlink(missing_ok=True)
            return
        wheels = wheel_cache(bundle, cache)
        fetch_components(bundle, cache, wheels)
        stage = BOOTSTRAP / 'ready'
        if stage.exists():
            if stage.resolve() != stage:
                raise ValueError('安装暂存目录不能指向其他位置。')
            shutil.rmtree(stage)
        stage.mkdir()
        site_packages = install_runtime(bundle, cache, wheels, stage)
        install_application(load_updater(site_packages), stage)
        (cache / 'error.txt').unlink(missing_ok=True)
        print('STEP 7', flush=True)
    except Exception as error:
        message = str(error)
        (cache / 'error.txt').write_text(message, encoding='utf-8')
        raise SystemExit(1) from None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=('prepare', 'install-vc', 'clean'))
    parser.add_argument('root', type=Path)
    parser.add_argument('--install-vc', action='store_true')
    args = parser.parse_args()
    root = args.root.absolute()
    if args.operation == 'prepare':
        prepare(root, args.install_vc)
    elif args.operation == 'install-vc':
        prepare(root, need_vc=True, vc_only=True)
    else:
        cache = cache_directory(root)
        if cache.is_dir() and (root / '.azurpilot/independent.json').is_file():
            shutil.rmtree(cache)


if __name__ == '__main__':
    main()

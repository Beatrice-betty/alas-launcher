"""Create the input directory for the Windows x64 light installer."""

import argparse
import email
import importlib.metadata
import shutil
import sys
import tomllib
from pathlib import Path
from urllib.parse import unquote, urlsplit

from packaging.tags import parse_tag, sys_tags
from packaging.utils import canonicalize_name, parse_wheel_filename

TOOLS = Path(__file__).resolve().parent
GIT_SHA256 = '4a92850b6e917da4fd010669b614b55dccf5b0ec333a534cc4be4700dfefdafd'
ADB_SHA256 = '45f4d63113e895ebde0c90f194099a4676b6ac653bd28d54314a9e022bbc1a99'
WEBVIEW2_SETUP_SHA256 = 'cf2f2445fef0c5c14e372045f84998771eeb8bb01d59be4afd00692fa160082e'
GIT = {
    'filename': 'MinGit-2.56.1-64-bit.zip',
    'sha256': GIT_SHA256,
    'urls': [
        'https://github.com/git-for-windows/git/releases/download/v2.56.1.windows.1/MinGit-2.56.1-64-bit.zip',
    ],
}
ADB = {
    'filename': 'platform-tools_r37.0.1-win.zip',
    'sha256': ADB_SHA256,
    'urls': ['https://dl.google.com/android/repository/platform-tools_r37.0.1-win.zip'],
}
VC_RUNTIME = {
    'filename': 'VC_redist.x64.exe',
    'sha256': '9c02c9ed120f671b900e6b9471d890e3b2414117a7e53f4ae2e855466fa2b7c9',
    'urls': [
        ('https://download.visualstudio.microsoft.com/download/pr/78d061c5-9ef3-42ab-98ed-3309d1095dc4/'
         '9C02C9ED120F671B900E6B9471D890E3B2414117A7E53F4AE2E855466FA2B7C9/VC_redist.x64.exe'),
    ],
}


def pinned_wheels(package, installed, local_wheel, bootstrap, digest):
    with (package / 'uv.lock').open('rb') as stream:
        packages = {(canonicalize_name(p['name']), p['version']): p for p in tomllib.load(stream)['package']}
    priority = {tag: index for index, tag in enumerate(sys_tags())}
    distributions = sorted(
        importlib.metadata.distributions(path=[str(installed)]),
        key=lambda dist: canonicalize_name(dist.metadata['Name']),
    )
    if not distributions or not any(tag.platform == 'win_amd64' for tag in priority):
        raise ValueError('请使用与整包相同版本的 Windows x64 Python 和已安装的生产依赖。')
    wheels = []
    requirements = []
    for dist in distributions:
        name, version = canonicalize_name(dist.metadata['Name']), dist.version
        locked = packages[(name, version)]
        installed_tags = set().union(*(parse_tag(tag) for tag in email.message_from_string(
            dist.read_text('WHEEL') or '',
        ).get_all('Tag', [])))
        matches = []
        for wheel in locked.get('wheels', []):
            path = urlsplit(wheel['url']).path
            filename = unquote(path.rsplit('/', 1)[-1])
            _, _, _, tags = parse_wheel_filename(filename)
            ranks = [priority[tag] for tag in tags & installed_tags if tag in priority]
            if ranks:
                matches.append((min(ranks), filename, wheel))
        if matches:
            _, filename, wheel = min(matches, key=lambda match: match[:2])
            url = wheel['url']
            path = urlsplit(url).path
            if not path.startswith('/pypi/packages/'):
                raise ValueError(f'锁文件来源格式不匹配 {name}')
            sha256 = wheel['hash'].removeprefix('sha256:')
            spec = {'filename': filename, 'sha256': sha256, 'urls': [
                url, 'https://files.pythonhosted.org' + path.removeprefix('/pypi'),
            ]}
        else:
            local_name, local_version, _, tags = parse_wheel_filename(local_wheel.name)
            if (local_name != name or str(local_version) != version or not tags & installed_tags
                    or not any(tag in priority for tag in tags)):
                raise ValueError(f'缺少锁定版本的可安装 wheel {name} {version}')
            target = bootstrap / 'wheels'
            target.mkdir(exist_ok=True)
            shutil.copy2(local_wheel, target)
            sha256 = digest(local_wheel)
            spec = {'filename': local_wheel.name, 'sha256': sha256, 'embedded': True}
        wheels.append(spec)
        requirements.append(f'{name}=={version} --hash=sha256:{sha256}')
    (bootstrap / 'requirements.txt').write_text('\n'.join(requirements) + '\n', encoding='utf-8')
    return wheels


def assemble(package, release, inputs, installed, local_wheel, webview2_setup, output):
    sys.path.insert(0, str(package))
    from build_independent import NODE_DIRECTORY, NODE_SHA256
    from deploy import independent

    receipt = independent.read_json(package / independent.RECEIPT)
    signed = independent.verify_manifest(independent.read_json(release / 'release.json'), receipt['publicKey'])
    if any(receipt.get(name) != value for name, value in signed.items()):
        raise ValueError('完整包和签名发布的版本不同。')
    if independent.dependency_id(package) != signed['dependencies']:
        raise ValueError('完整包的依赖锁与签名清单不一致。')
    if independent.digest(release / 'frontend.zip') != signed['frontendSha256']:
        raise ValueError('网页 ZIP 与签名清单不一致。')
    if independent.digest(inputs / 'mingit.zip') != GIT_SHA256 or independent.digest(inputs / 'adb.zip') != ADB_SHA256:
        raise ValueError('Git 或 ADB 的压缩包校验失败。')
    node_zip = inputs / (NODE_DIRECTORY + '.zip')
    if independent.digest(node_zip) != NODE_SHA256:
        raise ValueError('Node.js 官方发行包 SHA-256 不匹配')
    if independent.digest(webview2_setup) != WEBVIEW2_SETUP_SHA256:
        raise ValueError('微软 WebView2 引导程序 SHA-256 不匹配。')

    output.mkdir(parents=True, exist_ok=False)
    bootstrap = output / 'bootstrap'
    bootstrap.mkdir()
    for name in ('AzurPilot.exe', 'LICENSE', 'launcher-LICENSE.txt'):
        shutil.copy2(package / name, output)
    (output / '使用说明.txt').write_text(
        '装完简装包后，打开 AzurPilot，在启动器里点「部署」。\n'
        '组件和本体下载完成后点「启动」。以后的启动沿用已装环境，只更新 ALAS 和网页。\n'
        '下载失败可重试，也可点「导出启动器日志」。日志在安装目录的 log 文件夹。\n'
        '需要主动修复时，请先停服务，再点「修复环境」。控制台默认使用本机的 25549 端口。\n',
        encoding='utf-8',
    )

    python = package / 'runtime/python'

    def ignore_installed(directory, names):
        ignored = [name for name in names if name == '__pycache__' or name.endswith('.pyc')]
        if Path(directory) == python / 'Lib':
            ignored.append('site-packages')
        return ignored

    shutil.copytree(python, bootstrap / 'python', ignore=ignore_installed)
    shutil.copy2(webview2_setup, bootstrap / 'MicrosoftEdgeWebview2Setup.exe')
    shutil.copytree(package / 'runtime/atx', bootstrap / 'atx')
    shutil.copy2(package / 'runtime/components.json', bootstrap)
    shutil.copy2(package / 'config/deploy.yaml', bootstrap)
    shutil.copy2(package / 'deploy/independent.py', bootstrap)
    deploy = bootstrap / 'deploy'
    deploy.mkdir()
    for name in ('utils.py', 'atomic.py'):
        shutil.copy2(package / 'deploy' / name, deploy)
    shutil.copy2(package / independent.RECEIPT, bootstrap / 'receipt.json')
    shutil.copy2(TOOLS / 'bootstrap_independent_lite.py', bootstrap / 'bootstrap.py')
    wheels = pinned_wheels(package, installed, local_wheel, bootstrap, independent.digest)
    independent.write_json(bootstrap / 'downloads.json', {
        'git': GIT, 'adb': ADB, 'vc_runtime': VC_RUNTIME, 'wheels': wheels,
        'nodejs': {
            'filename': node_zip.name, 'size': node_zip.stat().st_size, 'sha256': NODE_SHA256,
            'urls': [f'{base}/v24.22.2/{node_zip.name}' for base in (
                'https://registry.npmmirror.com/-/binary/node',
                'https://mirrors.nju.edu.cn/nodejs-release',
                'https://nodejs.org/dist',
            )],
        },
        'uv': next(wheel for wheel in wheels if wheel['filename'].startswith('uv-')),
    })
    print(f'Windows runtime locked to {len(wheels)} Python packages, ALAS {signed["revision"]}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('package', 'release', 'inputs', 'installed', 'local-wheel', 'webview2-setup', 'output'):
        parser.add_argument('--' + name, type=Path, required=True)
    args = parser.parse_args()
    assemble(*(getattr(args, name).resolve() for name in (
        'package', 'release', 'inputs', 'installed', 'local_wheel', 'webview2_setup', 'output',
    )))


if __name__ == '__main__':
    main()

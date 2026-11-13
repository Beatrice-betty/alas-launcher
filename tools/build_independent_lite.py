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
WEBVIEW2 = {
    'filename': 'Microsoft.WebView2.FixedVersionRuntime.157.0.4341.46.x64.cab',
    'sha256': '0ac7317a08c3f3beebbb9b6d90f5b77352569f0a33a54a15ef6aaabc82899b7c',
    'urls': [
        'https://msedge.sf.dl.delivery.mp.microsoft.com/filestreamingservice/files/'
        'edb19202-9173-4dd1-ab87-c45c2d80aaec/Microsoft.WebView2.FixedVersionRuntime.157.0.4341.46.x64.cab',
    ],
}
VC_RUNTIME = {
    'filename': 'VC_redist.x64.exe',
    'sha256': '9c02c9ed120f671b900e6b9471d890e3b2414117a7e53f4ae2e855466fa2b7c9',
    'urls': [
        'https://download.visualstudio.microsoft.com/download/pr/78d061c5-9ef3-42ab-98ed-3309d1095dc4/'
        '9C02C9ED120F671B900E6B9471D890E3B2414117A7E53F4AE2E855466FA2B7C9/VC_redist.x64.exe',
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


def assemble(package, release, inputs, installed, local_wheel, output):
    sys.path.insert(0, str(package))
    from deploy import independent
    from build_independent import NODE_DIRECTORY, NODE_SHA256

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

    output.mkdir(parents=True, exist_ok=False)
    bootstrap = output / 'bootstrap'
    bootstrap.mkdir()
    for name in ('AzurPilot.exe', 'LICENSE', 'launcher-LICENSE.txt', '使用说明.txt'):
        shutil.copy2(package / name, output)

    python = package / 'runtime/python'

    def ignore_installed(directory, names):
        ignored = [name for name in names if name == '__pycache__' or name.endswith('.pyc')]
        if Path(directory) == python / 'Lib':
            ignored.append('site-packages')
        return ignored

    shutil.copytree(python, bootstrap / 'python', ignore=ignore_installed)
    shutil.copy2(inputs / 'mingit.zip', bootstrap / 'git.zip')
    shutil.copy2(inputs / 'adb.zip', bootstrap)
    shutil.copytree(package / 'runtime/atx', bootstrap / 'atx')
    shutil.copy2(package / 'runtime/components.json', bootstrap)
    shutil.copy2(package / 'config/deploy.yaml', bootstrap)
    shutil.copy2(package / 'deploy/independent.py', bootstrap)
    shutil.copy2(package / independent.RECEIPT, bootstrap / 'receipt.json')
    shutil.copy2(TOOLS / 'bootstrap_independent_lite.py', bootstrap / 'bootstrap.py')
    for name in ('release.json', 'frontend.zip'):
        shutil.copy2(release / name, bootstrap)
    wheels = pinned_wheels(package, installed, local_wheel, bootstrap, independent.digest)
    independent.write_json(bootstrap / 'downloads.json', {
        'webview2': WEBVIEW2, 'vc_runtime': VC_RUNTIME, 'wheels': wheels,
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
    for name in ('package', 'release', 'inputs', 'installed', 'local-wheel', 'output'):
        parser.add_argument('--' + name, type=Path, required=True)
    args = parser.parse_args()
    assemble(*(getattr(args, name).resolve() for name in (
        'package', 'release', 'inputs', 'installed', 'local_wheel', 'output',
    )))


if __name__ == '__main__':
    main()

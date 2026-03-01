#!/usr/bin/env python3
"""
打包脚本：将 server.py + client.html
打包成单个可执行文件（Windows: .exe / Linux/macOS: 无扩展名二进制）

用法：
    python build_exe.py

输出：
    dist/WebDesk[.exe]

依赖：
    pip install pyinstaller
"""

import subprocess
import sys
import os
import shutil

HERE = os.path.dirname(os.path.abspath(__file__))
SERVER_PY  = os.path.join(HERE, 'server.py')
CLIENT_HTML = os.path.join(HERE, 'client.html')


def check_files():
    for f in [SERVER_PY, CLIENT_HTML]:
        if not os.path.exists(f):
            print(f"❌ 找不到文件: {f}")
            sys.exit(1)
    print("✅ 源文件检查通过")


def ensure_pyinstaller():
    try:
        import PyInstaller
        print(f"✅ PyInstaller {PyInstaller.__version__} 已安装")
    except ImportError:
        print("📦 安装 PyInstaller...")
        subprocess.check_call([sys.executable, '-m', 'pip', 'install', 'pyinstaller'])


def build():
    check_files()
    ensure_pyinstaller()

    dist_dir  = os.path.join(HERE, 'dist')
    build_dir = os.path.join(HERE, 'build')
    spec_file = os.path.join(HERE, 'WebDesk.spec')

    # 清理旧产物
    for d in [dist_dir, build_dir]:
        if os.path.exists(d):
            shutil.rmtree(d)
            print(f"🗑  清理: {d}")
    if os.path.exists(spec_file):
        os.remove(spec_file)

    # --add-data 语法：PyInstaller 在 Windows 用分号，其他平台用冒号
    sep = ';' if sys.platform == 'win32' else ':'

    cmd = [
        sys.executable, '-m', 'PyInstaller',
        '--onefile',                          # 打包成单个文件
        '--name', 'WebDesk',            # 输出文件名
        '--noconfirm',                        # 不询问覆盖
        '--clean',                            # 清理缓存
        # 将 HTML 客户端嵌入包内，运行时解压到 sys._MEIPASS
        f'--add-data={CLIENT_HTML}{sep}.',
        # 隐式依赖（有些平台需要手动声明）
        '--hidden-import=PIL._imagingtk',
        '--hidden-import=PIL.Image',
        '--hidden-import=mss',
        '--hidden-import=numpy',
        '--hidden-import=websockets',
        '--hidden-import=pynput.mouse',
        '--hidden-import=pynput.keyboard',
        '--hidden-import=pynput._util',
    ]

    # Windows：隐藏控制台窗口（注释掉下面这行可保留控制台，方便调试）
    # if sys.platform == 'win32':
    #     cmd.append('--noconsole')

    # macOS：打包为 .app 的话改用 --windowed，这里保持命令行模式
    # if sys.platform == 'darwin':
    #     cmd.append('--windowed')

    cmd.append(SERVER_PY)

    print(f"\n🔨 执行打包命令:\n   {' '.join(cmd)}\n")
    result = subprocess.run(cmd, cwd=HERE)

    if result.returncode != 0:
        print("\n❌ 打包失败！请查看上方错误信息。")
        sys.exit(1)

    # 找到输出文件
    ext = '.exe' if sys.platform == 'win32' else ''
    output = os.path.join(dist_dir, f'WebDesk{ext}')
    if os.path.exists(output):
        size_mb = os.path.getsize(output) / 1024 / 1024
        print(f"\n✅ 打包成功！")
        print(f"   输出文件 : {output}")
        print(f"   文件大小 : {size_mb:.1f} MB")
        print(f"\n使用方法：")
        print(f"   直接双击运行，或命令行执行：")
        print(f"   ./dist/WebDesk{ext}")
    else:
        print(f"\n❌ 打包完成但找不到输出文件: {output}")
        sys.exit(1)


if __name__ == '__main__':
    build()

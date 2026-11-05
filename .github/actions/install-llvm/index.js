const core = require('@actions/core');
const exec = require('@actions/exec');
const tc = require('@actions/tool-cache');
const path = require('path');
const fs = require('fs');

const isWindows = process.platform == "win32"
const isMacOS = process.platform == "darwin"
const isLinux = process.platform == "linux"

async function execute(cmd) {
    let myOutput = '';
    let myError = '';
    await exec.exec(cmd, [], {
        listeners: {
            stdout: (data) => {
                myOutput += data.toString().trim();
            },
            stderr: (data) => {
                myError += data.toString().trim();
            }
        }
    });

    if (myError) {
        throw new Error(myError);
    }
    return myOutput;
}

(async () => {
    try {
        if (isLinux) {
            const installScript = path.join(__dirname, "../../../../scripts/install-llvm.sh");
            // run via sudo bash so the script doesn't need the executable bit set
            await exec.exec('sudo', ['bash', installScript]);
            // apt.llvm.org's packages put the unversioned executables (ld.lld,
            // llvm-config, ...) only in /usr/lib/llvm-22/bin. Exported as on
            // the other platforms: codira finds the LLD it spawns there or on
            // PATH.
            const llvmPath = "/usr/lib/llvm-22";
            core.addPath(`${llvmPath}/bin`)
            core.exportVariable('LLVM_SYS_221_PREFIX', llvmPath)
        } else if (isMacOS) {
            // Codira targets LLVM 22.1 (`inkwell`'s `llvm22-1` feature); llvm@14
            // is the wrong major version entirely and would fail the same way
            // building against llvm-sys as the Windows branch below did before
            // it pointed anywhere at all.
            await exec.exec("brew install llvm@22")
            let llvmPath = await execute("brew --prefix llvm@22");
            core.addPath(`${llvmPath}/bin`)
            core.exportVariable('LLVM_SYS_221_PREFIX', llvmPath)
            core.exportVariable('LIBCLANG_PATH', `${llvmPath}/lib`)
            // Homebrew ships LLD as a formula of its own, not in llvm@22: the
            // executables (ld64.lld links Codira's macOS libraries, ld.lld
            // its cross links) and the headers and static libraries
            // codira_lld builds against, which CODIRA_LLD_PREFIX points its
            // build script at.
            await exec.exec("brew install lld@22")
            let lldPath = await execute("brew --prefix lld@22");
            core.addPath(`${lldPath}/bin`)
            core.exportVariable('CODIRA_LLD_PREFIX', lldPath)
        } else if (isWindows) {
            // The previous URL pointed at `theomnira/llvm-package-windows`,
            // which does not exist (a 404, not a missing release) -- and even
            // fixed, the community-maintained repackaging this borrowed from
            // (vovkos/llvm-package-windows) has not published an LLVM 22
            // build. This downloads straight from the LLVM project's own
            // GitHub releases instead, which started shipping a full
            // Windows/MSVC dev archive (headers + static libs, the same
            // shape vovkos's builds provided) from LLVM 16 onward.
            const llvmVersion = "22.1.8"
            const downloadUrl = `https://github.com/llvm/llvm-project/releases/download/llvmorg-${llvmVersion}/clang+llvm-${llvmVersion}-x86_64-pc-windows-msvc.tar.xz`
            core.info(`downloading LLVM from '${downloadUrl}'`)
            const downloadLocation = await tc.downloadTool(downloadUrl);

            core.info("Succesfully downloaded LLVM release, extracting...")
            const llvmPath = "C:\\llvm22";
            fs.mkdirSync(llvmPath, { recursive: true });
            // `.tar.xz` rather than the old `.7z`, so this extracts with the
            // `tar` GitHub's Windows runners already ship (bsdtar, which
            // reads `.xz` compression natively) instead of the bundled
            // `7zr.exe`, which only understands the 7z container format.
            // `--one-top-level` is GNU tar only; bsdtar supports
            // `--strip-components`, which is enough since the archive has a
            // single top-level directory to drop.
            await exec.exec("tar", ["-xf", downloadLocation, "-C", llvmPath, "--strip-components=1"])

            core.addPath(`${llvmPath}\\bin`)
            core.exportVariable('LLVM_SYS_221_PREFIX', llvmPath)
            core.exportVariable('LIBCLANG_PATH', `${llvmPath}\\bin`)

            // The release's LLVM is linked against a static libxml2 (for
            // the manifest merging of LLVMWindowsManifest, which lld-link
            // uses), and llvm-config lists it as `xml2s.lib`, but the
            // archive does not ship it. vcpkg, which the runners have,
            // builds it with the same static CRT; it is copied in under the
            // name llvm-config gives.
            const vcpkg = process.env.VCPKG_INSTALLATION_ROOT
            await exec.exec(`${vcpkg}\\vcpkg.exe`, ["install", "libxml2[core]:x64-windows-static"])
            fs.copyFileSync(`${vcpkg}\\installed\\x64-windows-static\\lib\\libxml2.lib`, `${llvmPath}\\lib\\xml2s.lib`)
        } else {
            core.setFailed(`unsupported platform '${process.platform}'`)
        }
    } catch (error) {
        console.error(error.stack);
        core.setFailed(error.message);
    }
})();

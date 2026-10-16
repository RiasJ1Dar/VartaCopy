Modern File Explorer identity package
=====================================

This package gives the external x64 VartaCopy GUI and Explorer command DLL a
shared Windows package identity. The approved behavior is one selected ordinary
local folder becoming a source-only GUI draft. Registration covers ``Directory``
only. The DLL remains responsible for selection validation and for doing no copy
until the user explicitly chooses a destination and starts it.

Build without deployment
------------------------

Use Python 3.11+ and the Windows 10/11 SDK (MakeAppx, mt and SignTool). The SDK
default is 10.0.26100.0 x64; ``--sdk`` selects another SDK tool directory. Build
the real GUI and ``vartacopy_explorer.dll`` first; the tool rejects missing,
wrong-architecture and wrong-kind PE inputs. Each output directory must be new::

    python tools/modern_explorer.py build --gui target/x86_64-pc-windows-msvc/release/vartacopy-gui.exe --dll target/x86_64-pc-windows-msvc/release/vartacopy_explorer.dll --output artifacts/modern-explorer/unsigned-0.1.0
    python tools/test_modern_explorer.py

An optional independent ABI smoke loads the real release DLL without COM
registration, checks its exported factory, HRESULTs, references, title and
selection error paths, and never supplies a valid source to Invoke::

    python tools/test_modern_explorer.py --dll-smoke target/x86_64-pc-windows-msvc/release/vartacopy_explorer.dll

The tool creates a sparse ``VartaCopy.Explorer.msix``, inspectable package and
application manifests, original geometric PNG assets, a build/hash record and
an ``external`` directory containing copies of the GUI and DLL. No input binary
is modified. The artifact is explicitly ``unsigned-not-installable-until-signed``.
``MakeAppx /nv`` is required because binaries referenced by the sparse manifest
are external; a separate exact manifest/content check restricts the identity,
COM class, verb and Directory-only registration.

Executable identity and embedding
---------------------------------

The application manifest supplies ``msix`` publisher/packageName/applicationId
matching ``specs/explorer-identity-v1.json``. Packaging reads resource #1 using
``mt.exe``. If present, existing GUI manifest declarations are preserved and
conflicting MSIX identity is refused; absence of a resource is handled explicitly.
The resulting manifest is embedded in resource #1 of the staged GUI and extracted
again for identity verification. A sidecar remains as an inspectable source.
Deployment relies on the embedded manifest, so a pre-existing embedded manifest
cannot silently take precedence over an external identity sidecar. Packaging
supplies asInvoker, Windows 10/11 compatibility and PerMonitorV2 defaults where
the original manifest has no respective declaration.

Embedding changes the staged EXE hash and invalidates any pre-existing EXE
Authenticode signature. Input and staged hashes are recorded separately. If GUI
Authenticode signing is required, sign the final staged binary after embedding
and rebuild/update the release artifact through a controlled release process;
this tool's optional signing currently signs only the identity MSIX. The GUI
crate does not need a source/build-script change for this packaging approach.

Signing and explicit deployment
------------------------------

To build and sign in one operation, add ``--sign-thumbprint`` with the 40-hex
SHA1 thumbprint of an existing certificate in CurrentUser/My. SignTool uses an
explicit certificate selector, SHA256 digest and the current user's certificate
store. The certificate Subject must match ``CN=VartaCopy Development`` and its
chain must already be trusted on the target computer. The tool does not create,
import, export, print or change certificate/private-key material, trust stores,
DeveloperMode or policy. A failed signing/trust check fails the build; it does
not label the output installable. Production publisher and signing migration
are separate release decisions. There is no implicit timestamp service or
certificate selection::

    python tools/modern_explorer.py build --gui PATH/TO/vartacopy-gui.exe --dll PATH/TO/vartacopy_explorer.dll --output artifacts/modern-explorer/signed-0.1.0 --sign-thumbprint EXISTING_40_HEX_CERTIFICATE_THUMBPRINT
    python tools/modern_explorer.py status --output artifacts/modern-explorer/signed-0.1.0
    python tools/modern_explorer.py install --output artifacts/modern-explorer/signed-0.1.0
    python tools/modern_explorer.py remove --output artifacts/modern-explorer/signed-0.1.0

For self-signed development packages, MSIX deployment requires target machine
trust; Microsoft's package-signing instructions specify LocalMachine/TrustedPeople.
Arranging that trust is a separate administrator-approved setup action. Having
the signing certificate in CurrentUser/My or a passing SignTool Authenticode
verification does not by itself establish deployment trust for Add-AppxPackage.
The Appx deployment result remains authoritative and any failure is reported.
This tool never imports certificates or changes either user or machine trust.

Install requires intact recorded payloads, exact approved package contents and
a passing ``SignTool verify /pa /all`` trust check on the installation machine.
Keep the entire output directory at its final stable location before installing;
the external location is tied to that directory. Install refuses an existing
identity rather than replacing or taking it over. Operations use the current
user, with argument data passed separately from fixed PowerShell code.

Remove rechecks package name, publisher, x64 architecture, full name, current-user
SID and exact external directory. It uses the explicit output/external path and
does not require intact DLL, GUI, MSIX or build record, so quarantine or deleted
files do not prevent removal of the exact owned registration. It refuses machine
external registrations, reparse paths, ambiguous matches and other installation
directories, and leaves all files in place. Discovery is performed through
PackageManager for the current SID;
deployment uses the Windows Appx cmdlets and verifies the resulting registration.
There is no all-users provisioning, Explorer restart or classic-registry change.

Status keeps classic registration, modern registration, signature trust and
actual Shell activation separate. It also reports payload health independently
of registration: missing or changed files are damaged-or-missing; a missing MSIX
makes signature trust unknown, a failed verification is explicit, and an
unavailable signature tool is an error with unknown trust. Classic is explicitly not inspected by this
tool and must be queried through the existing classic integration command.
Registration success is not an Explorer activation test. No deployment or Shell
activation is performed by the unit tests. A live Windows 11 acceptance run on
a supported test machine must still verify menu visibility for one folder,
source-only draft delivery to the existing GUI, preserved history, cancellation
and absence of file transfer until an explicit start.

Acceptance on 2026-10-16
------------------------

A development-signed package was installed for the current user on Windows 11
x64 build 26300. The command appeared in the modern menu for one local NTFS
folder. Invocation opened a source-only draft with an empty destination;
cancellation started no transfer. Repeated invocation used the same GUI process.
An explicitly started copy preserved the test file and empty directory; the GUI
reported completion with verification and independent SHA256 comparison matched.
A later invocation retained history and cleared the old destination. The command
was absent for a selected file. Removal hid the command, and reinstallation
succeeded without restarting Explorer. These checks cover this development
machine and the stated Directory-only scope, not the full Windows test matrix.

The CLI continues to report ``shell_activation: not-tested`` because it does
not perform or infer an interactive acceptance test. Such evidence is recorded
separately from package registration and payload/signature health.

References
----------

* `Microsoft: Add a File Explorer context menu command <https://learn.microsoft.com/en-us/windows/apps/desktop/modernize/integrate-packaged-app-with-file-explorer>`_
* `Microsoft: Grant package identity with external location <https://learn.microsoft.com/en-us/windows/apps/desktop/modernize/grant-identity-to-nonpackaged-apps>`_
* `Microsoft: mt.exe <https://learn.microsoft.com/en-us/windows/win32/sbscs/mt-exe>`_
* `Microsoft: Package.EffectiveExternalPath <https://learn.microsoft.com/en-us/uwp/api/windows.applicationmodel.package.effectiveexternalpath>`_
* `Microsoft: Create a certificate for package signing <https://learn.microsoft.com/en-us/windows/msix/package/create-certificate-package-signing>`_

Optional REA artifact inspection
-------------------------------

REA is a development tool and is not a VartaCopy runtime dependency. The verified
CLI version for this workflow is 6.5.0 (Node.js 26.5.0). Install the CLI separately
from the VartaCopy repository and inspect the actual staged executable/package::

    npm install --global rea-agents@6.5.0 --ignore-scripts
    rea inspect-pe-resources ABSOLUTE/PATH/external/vartacopy-gui.exe --format json --full-output
    rea inspect-artifact ABSOLUTE/PATH/VartaCopy.Explorer.msix --format json --full-output
    rea inspect-rust-binary ABSOLUTE/PATH/external/vartacopy_explorer.dll --format json --full-output

Use ``inspect-artifact`` for the MSIX inventory; ``inspect`` requests a native
analysis overview and does not supply the same archive-only workflow. These
static commands require no Ghidra/Hopper/IDA installation and do not launch the
selected application. Preserve the complete Evidence, input hashes and returned
limitations outside the source repository. PE resource type 24/name 1 identifies
the embedded application manifest; its payload is opaque to the REA resource
parser, so separately parse the returned byte range to verify MSIX attributes.
Archive inventory and resource inspection do not verify signature trust or
Explorer activation. Missing ``.dep-v0`` Rust metadata means dependencies were
not recovered by this route; it does not establish that dependencies are absent.

REA analysis of third-party reference artifacts belongs to the black room.
White implementers receive only reviewed neutral requirements and evidence
appropriate to their role. REA availability does not enforce filesystem or
context isolation. Source-code navigation continues to use codebase-memory.

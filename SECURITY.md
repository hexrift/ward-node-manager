# Security policy

## Reporting a vulnerability

Please do not disclose security vulnerabilities in public issues. Use GitHub's
private vulnerability reporting for this repository when available. If it is
not enabled, contact the repository owner privately through GitHub and request
a secure reporting channel.

## Scope and limitations

WardNM currently runs disposable Linux containers with a shared host kernel. The
Docker daemon and host are trusted; this project does not claim a VM-equivalent
boundary or protection against kernel or daemon compromise. Report issues that
could escape the container, cross task boundaries, bypass configured limits, or
expose host data.

# Licensing

Lulan is an open-source project released under a dual-license model.

The goal is to keep the core reservation engine open while allowing developers and operators to build proprietary applications on top of it.

---

## Repository Licensing

| Component | License |
|-----------|---------|
| Core Engine | GNU Affero General Public License v3.0 (AGPL-3.0) |
| Rust Libraries | GNU Affero General Public License v3.0 (AGPL-3.0) |
| TypeScript SDKs | MIT License |
| UI Components | MIT License |
| Reference Applications | MIT License |
| Documentation | MIT License (unless otherwise noted) |

---

## Why AGPL?

The core of Lulan is licensed under the GNU Affero General Public License v3.0.

This ensures that:

- Improvements to the core remain open.
- Organizations offering Lulan as a hosted service must also contribute modifications to the core.
- The community benefits from long-term collaboration and transparency.

This helps prevent proprietary forks of the reservation engine while encouraging a healthy open-source ecosystem.

---

## Why MIT for SDKs?

SDKs, UI libraries, examples, and developer tooling are licensed under the MIT License.

This allows developers to:

- Build proprietary customer applications
- Integrate Lulan into commercial products
- Customize user interfaces without licensing concerns

Applications built **using** Lulan are **not required** to be open source solely because they use the MIT-licensed SDKs.

---

## Commercial Use

Commercial use of Lulan is permitted under the terms of the applicable licenses.

Organizations may:

- Self-host Lulan
- Build commercial products on top of Lulan
- Offer implementation and consulting services
- Develop proprietary frontends using the MIT-licensed SDKs

If you modify or distribute the AGPL-licensed core—or provide it as a network service—you must comply with the obligations of the AGPL.

Please review the GNU AGPL v3 before deploying modified versions of the core.

---

## Future Hosted Services

The maintainers may offer optional hosted or managed services in the future.

These services are separate from the open-source project and do not change the licensing of the source code.

---

## Third-Party Dependencies

Lulan depends on third-party open-source libraries that are licensed under their own terms.

Each dependency retains its original license.

See the package manifests (`Cargo.toml`, `package.json`, etc.) for details.

---

## Questions

If you have questions regarding licensing, commercial usage, or contributions, please open a GitHub Discussion or Issue.

---

## License Texts

The complete license texts are provided in:

- `LICENSE-AGPL`
- `LICENSE-MIT`

or within the individual packages where applicable.

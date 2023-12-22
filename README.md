# transform-exports

Fork of https://github.com/swc-project/plugins/tree/main/packages/transform-imports but for exports

## Config

```json
[
  "swc-plugin-transform-exports",
  {
    "react-bootstrap": {
      "transform": "react-bootstrap/lib/{{member}}"
    },
    "lodash": {
      "transform": "lodash/{{member}}"
    }
  }
]
```

pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Templates as T
import QtQuick.Layouts
import Qcm.Material as MD

// Renders one schema-driven control. The schema dict comes verbatim
// from `RendererPluginListQuery.renderers[i].settings[j]`; we never
// resolve `label_key`/`description_key` (no i18n yet) and fall back to
// a snake_case → Title Case transform of `key`.
ColumnLayout {
    id: root

    required property var schema
    // Stringified value (matches the wire format). When unset we use
    // schema.default_value so the form always has a base.
    property string value: ""

    // Named `committed` rather than `valueChanged` so it doesn't clash
    // with the `value` property's auto-generated notify signal.
    signal committed(string key, string newValue)

    spacing: 4

    readonly property int kU32: 1
    readonly property int kF32: 2
    readonly property int kString: 3
    readonly property int kBool: 4

    readonly property string label: {
        const sk = schema.label_key || "";
        if (sk.length > 0)
            return sk;
        const raw = schema.key || "";
        if (raw.length === 0)
            return "";
        return raw.split("_").map(function (p) {
            return p.length === 0 ? p : p[0].toUpperCase() + p.slice(1);
        }).join(" ");
    }

    readonly property string description: schema.description_key || ""
    readonly property bool needsRestart: schema.identity === true

    readonly property bool isTextField: {
        const t = schema.type;
        if (t === kBool)
            return false;
        if (t === kString)
            return !(schema.choices && schema.choices.length > 0);
        return !_hasNumericRange();
    }

    function _hasNumericRange() {
        const lo = schema.min || "";
        const hi = schema.max || "";
        return lo.length > 0 && hi.length > 0;
    }

    function _toFloat(s, fallback) {
        const n = parseFloat(s);
        return isNaN(n) ? fallback : n;
    }

    function _stepFor() {
        const s = schema.step || "";
        if (s.length > 0) {
            return root._toFloat(s, schema.type === root.kU32 ? 1 : 0.01);
        }
        return schema.type === root.kU32 ? 1 : 0.01;
    }

    function _emit(v) {
        root.value = v;
        root.committed(schema.key, v);
    }

    RowLayout {
        Layout.fillWidth: true
        spacing: 6
        visible: !root.isTextField || root.needsRestart

        MD.Text {
            Layout.fillWidth: true
            visible: !root.isTextField
            typescale: MD.Token.typescale.label_large
            color: MD.Token.color.on_surface
            text: root.label
        }

        // Identity-flag affordance: a small icon + tooltip warning the
        // user that changing this won't take effect on the live
        // renderer; daemon respawns it on next apply.
        MD.Icon {
            visible: root.needsRestart
            name: MD.Token.icon.restart_alt
            size: 16
            color: MD.Token.color.on_surface_variant
            T.ToolTip.visible: hovered.hovered
            T.ToolTip.text: "Requires renderer restart"

            HoverHandler {
                id: hovered
            }
        }
    }

    MD.Text {
        Layout.fillWidth: true
        visible: root.description.length > 0
        text: root.description
        typescale: MD.Token.typescale.body_small
        color: MD.Token.color.on_surface_variant
        wrapMode: Text.WordWrap
    }

    Loader {
        id: control
        Layout.fillWidth: true

        sourceComponent: {
            switch (root.schema.type) {
            case root.kBool:
                return boolField;
            case root.kU32:
            case root.kF32:
                return root._hasNumericRange() ? sliderField : numericField;
            case root.kString:
                if (root.schema.choices && root.schema.choices.length > 0)
                    return choiceField;
                return stringField;
            default:
                return stringField;
            }
        }
    }

    Component {
        id: boolField
        RowLayout {
            spacing: 8
            MD.Switch {
                checked: root.value === "true"
                onToggled: root._emit(checked ? "true" : "false")
            }
            Item {
                Layout.fillWidth: true
            }
        }
    }

    Component {
        id: sliderField
        RowLayout {
            spacing: 8
            readonly property real lo: root._toFloat(root.schema.min, 0)
            readonly property real hi: root._toFloat(root.schema.max, 1)

            MD.Slider {
                id: slider
                Layout.fillWidth: true
                from: parent.lo
                to: parent.hi
                stepSize: root._stepFor()
                snapMode: T.Slider.SnapAlways
                value: root._toFloat(root.value, parent.lo)
                onMoved: {
                    if (root.schema.type === root.kU32) {
                        root._emit(Math.round(value).toString());
                    } else {
                        root._emit(value.toString());
                    }
                }
            }

            MD.Text {
                Layout.preferredWidth: 56
                horizontalAlignment: Text.AlignRight
                typescale: MD.Token.typescale.label_medium
                text: root.schema.type === root.kU32 ? Math.round(slider.value).toString() : slider.value.toFixed(2)
            }
        }
    }

    Component {
        id: numericField
        MD.TextField {
            text: root.value
            placeholderText: root.label
            inputMethodHints: root.schema.type === root.kU32 ? Qt.ImhDigitsOnly : Qt.ImhFormattedNumbersOnly
            validator: root.schema.type === root.kU32 ? intValidator : doubleValidator
            onEditingFinished: root._emit(text)

            IntValidator {
                id: intValidator
                bottom: 0
            }
            DoubleValidator {
                id: doubleValidator
                notation: DoubleValidator.StandardNotation
            }
        }
    }

    Component {
        id: stringField
        MD.TextField {
            text: root.value
            placeholderText: root.label
            onEditingFinished: root._emit(text)
        }
    }

    Component {
        id: choiceField
        MD.ComboBox {
            model: root.schema.choices
            currentIndex: Math.max(0, root.schema.choices.indexOf(root.value))
            onActivated: root._emit(root.schema.choices[currentIndex])
        }
    }
}

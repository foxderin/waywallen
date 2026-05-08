pragma ValueTypeBehavior: Assertable
import QtQuick
import QtQuick.Templates as T
import QtQuick.Layouts
import Qcm.Material as MD
import waywallen.ui as W

// Compact colored chip identifying which GPU a renderer or display is on.
// Caller passes a DRM render-node id as (drmRenderMajor, drmRenderMinor);
// this component resolves it via App.gpuManager and picks a vendor color.
// Stays invisible until the GpuList fetch completes or when major == 0.
Rectangle {
    id: root

    property int drmRenderMajor: 0
    property int drmRenderMinor: 0

    readonly property var gpu: (root.drmRenderMajor > 0 && W.App.gpuManager)
        ? W.App.gpuManager.find(root.drmRenderMajor, root.drmRenderMinor)
        : null

    readonly property string label: {
        if (!root.gpu) return "";
        if (root.gpu.driver) return root.gpu.driver;
        return "drm:" + root.drmRenderMajor + ":" + root.drmRenderMinor;
    }

    // PCI vendor IDs: AMD 0x1002, NVIDIA 0x10de, Intel 0x8086.
    readonly property color vendorBg: {
        if (!root.gpu) return MD.Token.color.surface_container_high;
        const vid = root.gpu.vendorId;
        if (vid === 0x1002) return Qt.rgba(0.86, 0.20, 0.20, 1.0); // AMD red
        if (vid === 0x10de) return Qt.rgba(0.27, 0.66, 0.20, 1.0); // NVIDIA green
        if (vid === 0x8086) return Qt.rgba(0.20, 0.45, 0.85, 1.0); // Intel blue
        return MD.Token.color.tertiary;
    }
    readonly property color vendorFg: "white"

    visible: root.gpu !== null
    implicitWidth: tagText.implicitWidth + 16
    implicitHeight: tagText.implicitHeight + 6
    radius: height / 2
    color: root.vendorBg

    MD.Text {
        id: tagText
        anchors.centerIn: parent
        text: root.label
        typescale: MD.Token.typescale.label_small
        color: root.vendorFg
    }

    HoverHandler {
        id: hover
    }

    T.ToolTip.visible: hover.hovered && root.gpu !== null
    T.ToolTip.delay: 300
    T.ToolTip.text: root.gpu
        ? (root.gpu.description
            + (root.gpu.pciBdf ? "\nPCI " + root.gpu.pciBdf : "")
            + (root.gpu.renderNode ? "\n" + root.gpu.renderNode : ""))
        : ""
}

module;
#include "QExtra/macro_qt.hpp"

#ifdef Q_MOC_RUN
#    include "waywallen/util.moc"
#endif

export module waywallen:util;
export import qextra;

namespace waywallen
{

// QML singleton exposing miscellaneous UI helpers that are too heavy or
// regex-bound to write inline as QML JavaScript. Surfaces grow here as
// the QML side needs them.
//   * bbcodeToHtml(src) — Steam Workshop BBCode (used in WE
//     `project.json` descriptions) → Qt.StyledText HTML subset.
export class Util : public QObject {
    Q_OBJECT
    QML_ELEMENT
    QML_SINGLETON
public:
    explicit Util(QObject* parent);
    ~Util() override;
    Util() = delete;

    static Util* instance();
    static Util* create(QQmlEngine*, QJSEngine*);

    Q_INVOKABLE QString bbcodeToHtml(const QString& src) const;

    // WE wire-side color is `"r g b"` or `"r g b a"`, space-separated
    // 0-1 floats. Falls back to opaque black on malformed input.
    Q_INVOKABLE QColor  colorFromWire(const QString& s) const;
    Q_INVOKABLE QString colorToWire(const QColor& c, bool includeAlpha) const;
    Q_INVOKABLE bool    colorHasAlpha(const QString& s) const;
};

} // namespace waywallen

module;
#include "QExtra/macro_qt.hpp"

#ifdef Q_MOC_RUN
#    include "waywallen/query/display_query.moc"
#endif

export module waywallen:query.display;
export import :query.query;

namespace waywallen
{

export class DisplayListQuery : public Query, public QueryExtra<control::v1::Response, DisplayListQuery> {
    Q_OBJECT
    QML_ELEMENT

    Q_PROPERTY(QVariantList displays READ displays NOTIFY displaysChanged FINAL)

public:
    DisplayListQuery(QObject* parent = nullptr);

    auto displays() const -> const QVariantList&;

    void reload() override;

    Q_SIGNAL void displaysChanged();

private:
    QVariantList m_displays;
};

/// Mutate a single display's per-display layout override. Set
/// `fillmodeSet` (true) + `fillmode` (int FillMode enum) to write a
/// fillmode override; `clearFillmode = true` removes the override
/// (revert to global default). Same pattern for `align*`. Empty
/// `name` is rejected by the daemon. The daemon re-emits
/// `set_config` to the live consumer and broadcasts a
/// `DisplayChanged` event with the refreshed `effectiveLayout`.
///
/// Clear color is NOT exposed here — it's owned by the renderer.
export class DisplayLayoutSetQuery : public Query,
                                     public QueryExtra<control::v1::Response, DisplayLayoutSetQuery> {
    Q_OBJECT
    QML_ELEMENT

    Q_PROPERTY(QString name READ name WRITE setName NOTIFY paramsChanged FINAL)
    Q_PROPERTY(bool fillmodeSet READ fillmodeSet WRITE setFillmodeSet NOTIFY paramsChanged FINAL)
    Q_PROPERTY(int fillmode READ fillmode WRITE setFillmode NOTIFY paramsChanged FINAL)
    Q_PROPERTY(bool alignSet READ alignSet WRITE setAlignSet NOTIFY paramsChanged FINAL)
    Q_PROPERTY(int align READ align WRITE setAlign NOTIFY paramsChanged FINAL)
    Q_PROPERTY(bool clearFillmode READ clearFillmode WRITE setClearFillmode NOTIFY paramsChanged FINAL)
    Q_PROPERTY(bool clearAlign READ clearAlign WRITE setClearAlign NOTIFY paramsChanged FINAL)

public:
    DisplayLayoutSetQuery(QObject* parent = nullptr);

    auto name() const -> const QString& { return m_name; }
    void setName(const QString& v);
    auto fillmodeSet() const -> bool { return m_fillmode_set; }
    void setFillmodeSet(bool v);
    auto fillmode() const -> int { return m_fillmode; }
    void setFillmode(int v);
    auto alignSet() const -> bool { return m_align_set; }
    void setAlignSet(bool v);
    auto align() const -> int { return m_align; }
    void setAlign(int v);
    auto clearFillmode() const -> bool { return m_clear_fillmode; }
    void setClearFillmode(bool v);
    auto clearAlign() const -> bool { return m_clear_align; }
    void setClearAlign(bool v);

    void reload() override;

    Q_SIGNAL void paramsChanged();

private:
    QString      m_name;
    bool         m_fillmode_set { false };
    int          m_fillmode { 0 };
    bool         m_align_set { false };
    int          m_align { 0 };
    bool         m_clear_fillmode { false };
    bool         m_clear_align { false };
};

} // namespace waywallen

module;
#include "waywallen/util.moc.h"

module waywallen;
import :util;
import :app;

using namespace Qt::ops;

namespace waywallen
{

Util* Util::instance() {
    static Util* the = new Util(App::instance());
    return the;
}

Util* Util::create(QQmlEngine*, QJSEngine*) {
    auto t = instance();
    QJSEngine::setObjectOwnership(t, QJSEngine::CppOwnership);
    return t;
}

Util::Util(QObject* parent): QObject(parent) {}
Util::~Util() = default;

// --- BBCode → Qt StyledText HTML subset --------------------------------
//
// All regexes are static QRegularExpression so they compile once. PCRE
// `DotMatchesEverythingOption` lets `.*?` span newlines (Steam authors
// freely mix `\n` inside BBCode blocks).

namespace {

QString escapeHtml(QString s) {
    s.replace(QLatin1Char('&'), QLatin1StringView("&amp;"));
    s.replace(QLatin1Char('<'), QLatin1StringView("&lt;"));
    s.replace(QLatin1Char('>'), QLatin1StringView("&gt;"));
    return s;
}

const QRegularExpression& reNoparse() {
    static const QRegularExpression re(
        QStringLiteral("\\[noparse\\](.*?)\\[/noparse\\]"),
        QRegularExpression::CaseInsensitiveOption |
            QRegularExpression::DotMatchesEverythingOption);
    return re;
}

// Replace every `[tag]...[/tag]` occurrence with `open<capture>close`.
QString replaceTag(const QString& in, const QString& tag,
                   const QString& openHtml, const QString& closeHtml) {
    static QHash<QString, QRegularExpression*> cache;
    QString key = tag.toLower();
    auto it = cache.find(key);
    if (it == cache.end()) {
        auto* re = new QRegularExpression(
            QStringLiteral("\\[%1\\](.*?)\\[/%1\\]").arg(QRegularExpression::escape(tag)),
            QRegularExpression::CaseInsensitiveOption |
            QRegularExpression::DotMatchesEverythingOption);
        it = cache.insert(key, re);
    }
    return QString(in).replace(*it.value(), openHtml + QStringLiteral("\\1") + closeHtml);
}

} // namespace

QString Util::bbcodeToHtml(const QString& src) const {
    if (src.isEmpty()) return {};

    // Hoist [noparse] payloads behind opaque sentinels so the rest of the
    // pipeline doesn't touch their content. Restored after escaping.
    QStringList noparseStash;
    QString stage = src;
    {
        auto it = reNoparse().globalMatch(stage);
        QString out;
        out.reserve(stage.size());
        int cursor = 0;
        while (it.hasNext()) {
            auto m = it.next();
            out.append(stage.mid(cursor, m.capturedStart() - cursor));
            out.append(QStringLiteral("\x01NP%1\x01").arg(noparseStash.size()));
            noparseStash.append(m.captured(1));
            cursor = m.capturedEnd();
        }
        out.append(stage.mid(cursor));
        stage = std::move(out);
    }

    // Escape so the author can't smuggle raw HTML through. BBCode tag
    // rewrites below produce structural `<…>` directly.
    stage = escapeHtml(stage);

    // Inline marks.
    stage = replaceTag(stage, QStringLiteral("b"),
                       QStringLiteral("<b>"),  QStringLiteral("</b>"));
    stage = replaceTag(stage, QStringLiteral("i"),
                       QStringLiteral("<i>"),  QStringLiteral("</i>"));
    stage = replaceTag(stage, QStringLiteral("u"),
                       QStringLiteral("<u>"),  QStringLiteral("</u>"));
    stage = replaceTag(stage, QStringLiteral("s"),
                       QStringLiteral("<s>"),  QStringLiteral("</s>"));
    stage = replaceTag(stage, QStringLiteral("strike"),
                       QStringLiteral("<s>"),  QStringLiteral("</s>"));
    stage = replaceTag(stage, QStringLiteral("code"),
                       QStringLiteral("<tt>"), QStringLiteral("</tt>"));
    stage = replaceTag(stage, QStringLiteral("spoiler"),
                       QStringLiteral("<font color=\"#888888\">"),
                       QStringLiteral("</font>"));

    // [quote] / [quote=who] → italic-quoted text. quote=who's source attr
    // isn't surfaced (no good spot in StyledText) — strip and keep payload.
    {
        static const QRegularExpression re(
            QStringLiteral("\\[quote(?:=[^\\]]*)?\\](.*?)\\[/quote\\]"),
            QRegularExpression::CaseInsensitiveOption |
            QRegularExpression::DotMatchesEverythingOption);
        stage.replace(re, QStringLiteral("<i>“\\1”</i>"));
    }

    // Headings — drop a tier so Workshop's [h1] doesn't dwarf the panel.
    stage = replaceTag(stage, QStringLiteral("h1"),
                       QStringLiteral("<h2>"), QStringLiteral("</h2>"));
    stage = replaceTag(stage, QStringLiteral("h2"),
                       QStringLiteral("<h3>"), QStringLiteral("</h3>"));
    stage = replaceTag(stage, QStringLiteral("h3"),
                       QStringLiteral("<h4>"), QStringLiteral("</h4>"));

    // Rules + lists. [*] terminator is implicit; emit <li> openers and let
    // the surrounding <ul>…</ul> auto-close items at </ul>.
    {
        static const QRegularExpression reHr(QStringLiteral("\\[hr\\]"),
                                             QRegularExpression::CaseInsensitiveOption);
        stage.replace(reHr, QStringLiteral("<hr>"));
        static const QRegularExpression reListOpen(QStringLiteral("\\[list\\]"),
                                                   QRegularExpression::CaseInsensitiveOption);
        stage.replace(reListOpen, QStringLiteral("<ul>"));
        static const QRegularExpression reListClose(QStringLiteral("\\[/list\\]"),
                                                    QRegularExpression::CaseInsensitiveOption);
        stage.replace(reListClose, QStringLiteral("</ul>"));
        static const QRegularExpression reItem(QStringLiteral("\\[\\*\\]\\s*"));
        stage.replace(reItem, QStringLiteral("<li>"));
    }

    // Links: [url=X]Y[/url] then [url]X[/url]. img: [img]X[/img]. Qt won't
    // fetch http(s) imgs without a network image provider; a missing-image
    // glyph still hints at presence.
    {
        static const QRegularExpression reUrlEq(
            QStringLiteral("\\[url=([\"']?)([^\\]\"']+)\\1\\](.*?)\\[/url\\]"),
            QRegularExpression::CaseInsensitiveOption |
            QRegularExpression::DotMatchesEverythingOption);
        stage.replace(reUrlEq, QStringLiteral("<a href=\"\\2\">\\3</a>"));

        static const QRegularExpression reUrl(
            QStringLiteral("\\[url\\](.*?)\\[/url\\]"),
            QRegularExpression::CaseInsensitiveOption |
            QRegularExpression::DotMatchesEverythingOption);
        stage.replace(reUrl, QStringLiteral("<a href=\"\\1\">\\1</a>"));

        static const QRegularExpression reImg(
            QStringLiteral("\\[img\\](.*?)\\[/img\\]"),
            QRegularExpression::CaseInsensitiveOption |
            QRegularExpression::DotMatchesEverythingOption);
        stage.replace(reImg, QStringLiteral("<img src=\"\\1\">"));
    }

    // Auto-linkify bare URLs. Single sweep that either matches a `href="`
    // marker (and leaves it alone) or a free-standing URL (and wraps it).
    {
        static const QRegularExpression reAuto(
            QStringLiteral("(href=\")|(https?://[^\\s<\\[\\]]+)"));
        QString out;
        out.reserve(stage.size());
        int cursor = 0;
        auto it = reAuto.globalMatch(stage);
        while (it.hasNext()) {
            auto m = it.next();
            out.append(stage.mid(cursor, m.capturedStart() - cursor));
            if (m.capturedLength(1) > 0) {
                // It's an existing href="…" — skip.
                out.append(m.captured(0));
            } else {
                const QString url = m.captured(2);
                out.append(QStringLiteral("<a href=\"%1\">%1</a>").arg(url));
            }
            cursor = m.capturedEnd();
        }
        out.append(stage.mid(cursor));
        stage = std::move(out);
    }

    // Line breaks last so embedded \n inside tag payloads stayed intact
    // through the scans above.
    stage.replace(QStringLiteral("\r\n"), QStringLiteral("\n"));
    stage.replace(QStringLiteral("\n"), QStringLiteral("<br>"));

    // Restore [noparse] payloads as escaped, untouched text.
    {
        static const QRegularExpression reNp(QStringLiteral("\x01NP(\\d+)\x01"));
        QString out;
        out.reserve(stage.size());
        int cursor = 0;
        auto it = reNp.globalMatch(stage);
        while (it.hasNext()) {
            auto m = it.next();
            out.append(stage.mid(cursor, m.capturedStart() - cursor));
            const int idx = m.captured(1).toInt();
            if (idx >= 0 && idx < noparseStash.size()) {
                out.append(escapeHtml(noparseStash.at(idx)));
            }
            cursor = m.capturedEnd();
        }
        out.append(stage.mid(cursor));
        stage = std::move(out);
    }

    return stage;
}

// --- WE wire color round-trip --------------------------------------------

namespace {

QList<double> parseWireColor(const QString& s) {
    QList<double> out;
    static const QRegularExpression reSpaces(QStringLiteral("\\s+"));
    const auto parts = s.trimmed().split(reSpaces, Qt::SkipEmptyParts);
    out.reserve(parts.size());
    for (const auto& p : parts) {
        bool ok = false;
        const double v = p.toDouble(&ok);
        if (! ok) return {};
        out.append(v);
    }
    return out;
}

double clamp01(double v) {
    if (v < 0.0) return 0.0;
    if (v > 1.0) return 1.0;
    return v;
}

} // namespace

QColor Util::colorFromWire(const QString& s) const {
    const auto nums = parseWireColor(s);
    if (nums.size() < 3) return QColor::fromRgbF(0.0f, 0.0f, 0.0f, 1.0f);
    const double a = nums.size() >= 4 ? clamp01(nums[3]) : 1.0;
    return QColor::fromRgbF(static_cast<float>(clamp01(nums[0])),
                            static_cast<float>(clamp01(nums[1])),
                            static_cast<float>(clamp01(nums[2])),
                            static_cast<float>(a));
}

QString Util::colorToWire(const QColor& c, bool includeAlpha) const {
    const QString r = QString::number(c.redF(),   'f', 4);
    const QString g = QString::number(c.greenF(), 'f', 4);
    const QString b = QString::number(c.blueF(),  'f', 4);
    if (! includeAlpha) return r + QLatin1Char(' ') + g + QLatin1Char(' ') + b;
    const QString a = QString::number(c.alphaF(), 'f', 4);
    return r + QLatin1Char(' ') + g + QLatin1Char(' ') + b + QLatin1Char(' ') + a;
}

bool Util::colorHasAlpha(const QString& s) const {
    return parseWireColor(s).size() >= 4;
}

} // namespace waywallen

#include "waywallen/util.moc.cpp"

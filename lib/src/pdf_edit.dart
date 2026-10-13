// Phase 8: what filling forms, annotating and stamping PDFs takes and
// gives. Positions are displayed page points with a top-left origin, as
// the text layout uses; pages are 1-based.

/// A rectangle in displayed page points.
class PdfBox {
  const PdfBox(this.left, this.top, this.right, this.bottom);

  factory PdfBox.fromJson(Object? json) {
    if (json is! List || json.length != 4) {
      throw const FormatException('Invalid PDF rectangle.');
    }
    final v = [for (final n in json) _number(n, 'rectangle')];
    return PdfBox(v[0], v[1], v[2], v[3]);
  }

  final double left;
  final double top;
  final double right;
  final double bottom;

  double get width => right - left;
  double get height => bottom - top;

  List<double> toJson() => [left, top, right, bottom];

  @override
  bool operator ==(Object other) =>
      other is PdfBox &&
      other.left == left &&
      other.top == top &&
      other.right == right &&
      other.bottom == bottom;

  @override
  int get hashCode => Object.hash(left, top, right, bottom);

  @override
  String toString() => 'PdfBox($left, $top, $right, $bottom)';
}

double _number(Object? v, String what) {
  if (v is num && v.isFinite) return v.toDouble();
  throw FormatException('Invalid $what.');
}

/// An ARGB colour (as Flutter's `Color.toARGB32()` gives it) as "#RRGGBB".
String _hex(int argb) =>
    '#${(argb & 0xFFFFFF).toRadixString(16).padLeft(6, '0').toUpperCase()}';

int? _argb(Object? hex) {
  if (hex is! String || hex.length != 7 || !hex.startsWith('#')) return null;
  final rgb = int.tryParse(hex.substring(1), radix: 16);
  return rgb == null ? null : 0xFF000000 | rgb;
}

// -- forms -------------------------------------------------------------------

enum PdfFieldKind { text, checkbox, radio, choice, signature, button }

class PdfChoiceOption {
  const PdfChoiceOption(this.value, this.label);

  /// What is stored when this option is chosen.
  final String value;

  /// What is shown; the value when the form gives no label.
  final String label;
}

/// Where a field is drawn: one of its widgets.
class PdfFieldWidget {
  const PdfFieldWidget({
    required this.page,
    required this.box,
    this.onState,
    this.fontSize = 0,
  });

  final int page;
  final PdfBox box;

  /// The state this check box or radio button stands for.
  final String? onState;

  /// The text size the form asks for; 0 means "fit the box".
  final double fontSize;
}

/// A field of a PDF form, by its full name.
class PdfFormField {
  const PdfFormField({
    required this.name,
    required this.kind,
    required this.value,
    this.values = const [],
    this.options = const [],
    this.states = const [],
    this.readOnly = false,
    this.required = false,
    this.multiline = false,
    this.password = false,
    this.comb = false,
    this.combo = false,
    this.editable = false,
    this.multiSelect = false,
    this.maxLength,
    this.tooltip = '',
    this.widgets = const [],
  });

  factory PdfFormField.fromJson(Map<String, dynamic> json) {
    final kind = PdfFieldKind.values.asNameMap()[json['kind']];
    final name = json['name'];
    if (kind == null || name is! String) {
      throw const FormatException('Invalid PDF form field.');
    }
    List<String> strings(Object? v) =>
        v is List ? [for (final s in v) s.toString()] : const [];
    return PdfFormField(
      name: name,
      kind: kind,
      value: (json['value'] as String?) ?? '',
      values: strings(json['values']),
      options: [
        for (final o in (json['options'] as List? ?? const []))
          PdfChoiceOption(
            (o as Map)['value'].toString(),
            o['label'].toString(),
          ),
      ],
      states: strings(json['states']),
      readOnly: json['readOnly'] == true,
      required: json['required'] == true,
      multiline: json['multiline'] == true,
      password: json['password'] == true,
      comb: json['comb'] == true,
      combo: json['combo'] == true,
      editable: json['editable'] == true,
      multiSelect: json['multiSelect'] == true,
      maxLength: json['maxLength'] as int?,
      tooltip: (json['tooltip'] as String?) ?? '',
      widgets: [
        for (final w in (json['widgets'] as List? ?? const []))
          PdfFieldWidget(
            page: (w as Map)['page'] as int,
            box: PdfBox.fromJson(w['rect']),
            onState: w['onState'] as String?,
            fontSize: _number(w['fontSize'] ?? 0, 'font size'),
          ),
      ],
    );
  }

  final String name;
  final PdfFieldKind kind;

  /// Text, the chosen option, or a button's state ("Off" when clear).
  final String value;

  /// Every chosen option of a list that takes several.
  final List<String> values;
  final List<PdfChoiceOption> options;

  /// The states a check box or radio group can be in, besides "Off".
  final List<String> states;
  final bool readOnly;
  final bool required;
  final bool multiline;
  final bool password;
  final bool comb;

  /// A drop-down rather than a list.
  final bool combo;

  /// A drop-down that also takes typed text.
  final bool editable;
  final bool multiSelect;
  final int? maxLength;

  /// The form's description of the field.
  final String tooltip;
  final List<PdfFieldWidget> widgets;

  bool get isChecked =>
      (kind == PdfFieldKind.checkbox || kind == PdfFieldKind.radio) &&
      value.isNotEmpty &&
      value != 'Off';

  /// Whether the app can fill it.
  bool get fillable =>
      !readOnly &&
      kind != PdfFieldKind.signature &&
      kind != PdfFieldKind.button;
}

/// A document's form.
class PdfForm {
  const PdfForm({this.fields = const [], this.xfa = false, this.signed = false});

  factory PdfForm.fromJson(Map<String, dynamic> json) => PdfForm(
        fields: [
          for (final f in (json['fields'] as List? ?? const []))
            PdfFormField.fromJson(f as Map<String, dynamic>),
        ],
        xfa: json['xfa'] == true,
        signed: json['signed'] == true,
      );

  final List<PdfFormField> fields;

  /// The form also has an XFA description; filling it drops that, so
  /// readers show the values written here.
  final bool xfa;

  /// A signature field is signed. Saving appends to the file, so the
  /// signature stays valid.
  final bool signed;

  bool get isEmpty => fields.isEmpty;
}

/// A new value for a field.
class PdfFieldValue {
  /// Text, a drop-down's choice, or a radio group's state (or option).
  const PdfFieldValue.text(this.name, String this.value)
      : checked = null,
        selected = null;

  const PdfFieldValue.checked(this.name, bool this.checked)
      : value = null,
        selected = null;

  /// The chosen options of a list.
  const PdfFieldValue.selected(this.name, List<String> this.selected)
      : value = null,
        checked = null;

  final String name;
  final String? value;
  final bool? checked;
  final List<String>? selected;

  Map<String, Object?> toJson() => {
        'name': name,
        if (value != null) 'value': value,
        if (checked != null) 'checked': checked,
        if (selected != null) 'selected': selected,
      };
}

// -- annotations ---------------------------------------------------------------

/// An annotation already on a page.
class PdfAnnotationInfo {
  const PdfAnnotationInfo({
    required this.id,
    required this.subtype,
    required this.box,
    this.contents = '',
    this.author = '',
    this.color,
    this.ours = false,
  });

  factory PdfAnnotationInfo.fromJson(Map<String, dynamic> json) =>
      PdfAnnotationInfo(
        id: json['id'] as String,
        subtype: json['subtype'] as String,
        box: PdfBox.fromJson(json['rect']),
        contents: (json['contents'] as String?) ?? '',
        author: (json['author'] as String?) ?? '',
        color: _argb(json['color']),
        ours: json['ours'] == true,
      );

  /// What [PdfAnnotationRemoval] takes.
  final String id;

  /// The PDF subtype: Highlight, Ink, FreeText, Text (a note), Stamp, Link…
  final String subtype;
  final PdfBox box;
  final String contents;
  final String author;
  final int? color;

  /// Added by this app.
  final bool ours;

  /// Comments people make; links and form widgets are not.
  bool get isMarkup => !const {
        'Link',
        'Widget',
        'Popup',
        'Movie',
        'Screen',
        'PrinterMark',
        'TrapNet',
        'Watermark',
        '3D',
      }.contains(subtype);
}

/// A picture for an image annotation or mark: a JPEG file as it is, or a
/// file of raw RGBA pixels (straight alpha, `ImageByteFormat.rawStraightRgba`).
class PdfImageSource {
  const PdfImageSource.jpeg(this.path)
      : width = 0,
        height = 0;

  const PdfImageSource.rgba(this.path, this.width, this.height);

  final String path;
  final int width;
  final int height;

  Map<String, Object> toJson() => {
        'path': path,
        if (width > 0) 'width': width,
        if (height > 0) 'height': height,
      };
}

enum PdfAnnotationType {
  highlight,
  underline,
  strikeOut,
  squiggly,
  ink,
  rectangle,
  ellipse,
  line,
  arrow,
  freeText,
  text,
  note,
  image,
  signature,
}

typedef PdfPoint = (double, double);

List<List<double>> _points(List<PdfPoint> points) =>
    [for (final p in points) [p.$1, p.$2]];

/// One annotation to add. With [flatten] it is drawn into the page for
/// good instead.
class PdfAnnotationEdit {
  const PdfAnnotationEdit._({
    required this.page,
    required this.type,
    this.rects = const [],
    this.strokes = const [],
    this.box,
    this.from,
    this.to,
    this.text = '',
    this.fontSize = 12,
    this.image,
    this.color = 0xFF000000,
    this.fill,
    this.width = 1,
    this.opacity = 1,
    this.contents = '',
    this.author = '',
    this.flatten = false,
  });

  /// Highlight, underline, strike-out or squiggly over text: one box per
  /// line of the selection.
  factory PdfAnnotationEdit.markup(
    PdfAnnotationType type, {
    required int page,
    required List<PdfBox> rects,
    int color = 0xFFFFEB3B,
    double opacity = 1,
    String contents = '',
    String author = '',
  }) {
    assert(const {
      PdfAnnotationType.highlight,
      PdfAnnotationType.underline,
      PdfAnnotationType.strikeOut,
      PdfAnnotationType.squiggly,
    }.contains(type));
    return PdfAnnotationEdit._(
      page: page,
      type: type,
      rects: rects,
      color: color,
      opacity: opacity,
      contents: contents,
      author: author,
    );
  }

  /// Freehand drawing.
  factory PdfAnnotationEdit.ink({
    required int page,
    required List<List<PdfPoint>> strokes,
    int color = 0xFF1565C0,
    double width = 2,
    double opacity = 1,
    String author = '',
    bool flatten = false,
  }) =>
      PdfAnnotationEdit._(
        page: page,
        type: PdfAnnotationType.ink,
        strokes: strokes,
        color: color,
        width: width,
        opacity: opacity,
        author: author,
        flatten: flatten,
      );

  /// A rectangle or an ellipse in [box].
  factory PdfAnnotationEdit.shape({
    required int page,
    required PdfBox box,
    bool ellipse = false,
    int color = 0xFFE53935,
    int? fill,
    double width = 2,
    double opacity = 1,
    String author = '',
    bool flatten = false,
  }) =>
      PdfAnnotationEdit._(
        page: page,
        type: ellipse ? PdfAnnotationType.ellipse : PdfAnnotationType.rectangle,
        box: box,
        color: color,
        fill: fill,
        width: width,
        opacity: opacity,
        author: author,
        flatten: flatten,
      );

  /// A line, with an arrowhead at [to] when [arrow].
  factory PdfAnnotationEdit.line({
    required int page,
    required PdfPoint from,
    required PdfPoint to,
    bool arrow = false,
    int color = 0xFFE53935,
    double width = 2,
    double opacity = 1,
    String author = '',
    bool flatten = false,
  }) =>
      PdfAnnotationEdit._(
        page: page,
        type: arrow ? PdfAnnotationType.arrow : PdfAnnotationType.line,
        from: from,
        to: to,
        color: color,
        width: width,
        opacity: opacity,
        author: author,
        flatten: flatten,
      );

  /// A text box. Without a [border] it is typed text, as when filling in a
  /// form that has no fields.
  factory PdfAnnotationEdit.textBox({
    required int page,
    required PdfBox box,
    required String text,
    double fontSize = 12,
    int color = 0xFF000000,
    int? fill,
    double border = 1,
    String author = '',
    bool flatten = false,
  }) =>
      PdfAnnotationEdit._(
        page: page,
        type: border > 0 ? PdfAnnotationType.freeText : PdfAnnotationType.text,
        box: box,
        text: text,
        fontSize: fontSize,
        color: color,
        fill: fill,
        width: border,
        author: author,
        flatten: flatten,
      );

  /// A sticky note whose icon's top-left corner is [at].
  factory PdfAnnotationEdit.note({
    required int page,
    required PdfPoint at,
    required String text,
    int color = 0xFFFFC107,
    String author = '',
  }) =>
      PdfAnnotationEdit._(
        page: page,
        type: PdfAnnotationType.note,
        from: at,
        text: text,
        color: color,
        author: author,
      );

  /// A picture in [box]: a photo, a scanned signature, a stamp.
  factory PdfAnnotationEdit.image({
    required int page,
    required PdfBox box,
    required PdfImageSource image,
    double opacity = 1,
    String author = '',
    bool flatten = false,
  }) =>
      PdfAnnotationEdit._(
        page: page,
        type: PdfAnnotationType.image,
        box: box,
        image: image,
        opacity: opacity,
        author: author,
        flatten: flatten,
      );

  /// A drawn signature, its strokes in page points.
  factory PdfAnnotationEdit.signature({
    required int page,
    required PdfBox box,
    required List<List<PdfPoint>> strokes,
    int color = 0xFF0D47A1,
    double width = 2,
    String author = '',
    bool flatten = false,
  }) =>
      PdfAnnotationEdit._(
        page: page,
        type: PdfAnnotationType.signature,
        box: box,
        strokes: strokes,
        color: color,
        width: width,
        author: author,
        flatten: flatten,
      );

  final int page;
  final PdfAnnotationType type;
  final List<PdfBox> rects;
  final List<List<PdfPoint>> strokes;
  final PdfBox? box;
  final PdfPoint? from;
  final PdfPoint? to;
  final String text;
  final double fontSize;
  final PdfImageSource? image;

  /// ARGB; the alpha is ignored (see [opacity]).
  final int color;
  final int? fill;
  final double width;
  final double opacity;
  final String contents;
  final String author;
  final bool flatten;

  Map<String, Object?> toJson() {
    final json = <String, Object?>{
      'page': page,
      'type': type.name,
      'color': _hex(color),
      if (fill != null) 'fill': _hex(fill!),
      'width': width,
      'opacity': opacity,
      if (contents.isNotEmpty) 'contents': contents,
      if (author.isNotEmpty) 'author': author,
      if (flatten) 'flatten': true,
    };
    switch (type) {
      case PdfAnnotationType.highlight:
      case PdfAnnotationType.underline:
      case PdfAnnotationType.strikeOut:
      case PdfAnnotationType.squiggly:
        json['rects'] = [for (final r in rects) r.toJson()];
      case PdfAnnotationType.ink:
        json['strokes'] = [for (final s in strokes) _points(s)];
      case PdfAnnotationType.rectangle:
      case PdfAnnotationType.ellipse:
        json['rect'] = box!.toJson();
      case PdfAnnotationType.line:
      case PdfAnnotationType.arrow:
        json['from'] = [from!.$1, from!.$2];
        json['to'] = [to!.$1, to!.$2];
      case PdfAnnotationType.freeText:
      case PdfAnnotationType.text:
        json['rect'] = box!.toJson();
        json['text'] = text;
        json['fontSize'] = fontSize;
      case PdfAnnotationType.note:
        json['at'] = [from!.$1, from!.$2];
        json['text'] = text;
      case PdfAnnotationType.image:
        json['rect'] = box!.toJson();
        json['image'] = image!.toJson();
      case PdfAnnotationType.signature:
        json['rect'] = box!.toJson();
        json['strokes'] = [for (final s in strokes) _points(s)];
    }
    return json;
  }
}

/// An annotation to remove.
class PdfAnnotationRemoval {
  const PdfAnnotationRemoval(this.page, this.id);

  final int page;
  final String id;

  Map<String, Object> toJson() => {'page': page, 'id': id};
}

// -- marks ---------------------------------------------------------------------

enum PdfMarkPosition {
  topLeft,
  topCenter,
  topRight,
  center,
  bottomLeft,
  bottomCenter,
  bottomRight,
}

enum PdfMarkFont { sans, serif, mono }

/// Text or a picture drawn onto pages: page numbers, headers and footers,
/// watermarks. In [text], `{page}` is the page's number and `{pages}` the
/// count.
class PdfPageMark {
  const PdfPageMark({
    this.pages = const [],
    this.text = '',
    this.image,
    this.size,
    this.position = PdfMarkPosition.bottomCenter,
    this.fontSize = 12,
    this.font = PdfMarkFont.sans,
    this.bold = false,
    this.color = 0xFF000000,
    this.opacity = 1,
    this.rotation = 0,
    this.margin = 28,
  });

  /// Page numbers like "Page 3 of 10" at the bottom of every page.
  const PdfPageMark.pageNumbers({
    String format = '{page}',
    PdfMarkPosition position = PdfMarkPosition.bottomCenter,
    double fontSize = 10,
    List<int> pages = const [],
  }) : this(
          text: format,
          position: position,
          fontSize: fontSize,
          pages: pages,
        );

  /// A large, faint word across the middle of each page.
  const PdfPageMark.watermark(
    String text, {
    int color = 0xFFE53935,
    double opacity = 0.2,
    double fontSize = 72,
    double rotation = 45,
    List<int> pages = const [],
  }) : this(
          text: text,
          position: PdfMarkPosition.center,
          color: color,
          opacity: opacity,
          fontSize: fontSize,
          rotation: rotation,
          bold: true,
          pages: pages,
        );

  /// 1-based; empty means every page.
  final List<int> pages;
  final String text;
  final PdfImageSource? image;

  /// The picture's size in points; its own proportions when null.
  final (double, double)? size;
  final PdfMarkPosition position;
  final double fontSize;
  final PdfMarkFont font;
  final bool bold;
  final int color;
  final double opacity;

  /// Degrees, anticlockwise as the page is shown.
  final double rotation;

  /// Distance from the page's edges in points.
  final double margin;

  Map<String, Object?> toJson() => {
        if (pages.isNotEmpty) 'pages': pages,
        if (text.isNotEmpty) 'text': text,
        if (image != null) 'image': image!.toJson(),
        if (size != null) 'size': [size!.$1, size!.$2],
        'position': position.name,
        'fontSize': fontSize,
        'font': font.name,
        'bold': bold,
        'color': _hex(color),
        'opacity': opacity,
        'rotation': rotation,
        'margin': margin,
      };
}

/// Everything one save changes.
class PdfEdits {
  const PdfEdits({
    this.fields = const [],
    this.annotations = const [],
    this.remove = const [],
    this.marks = const [],
  });

  final List<PdfFieldValue> fields;
  final List<PdfAnnotationEdit> annotations;
  final List<PdfAnnotationRemoval> remove;
  final List<PdfPageMark> marks;

  bool get isEmpty =>
      fields.isEmpty && annotations.isEmpty && remove.isEmpty && marks.isEmpty;

  Map<String, Object> toJson() => {
        if (fields.isNotEmpty) 'fields': [for (final f in fields) f.toJson()],
        if (annotations.isNotEmpty)
          'annotations': [for (final a in annotations) a.toJson()],
        if (remove.isNotEmpty) 'delete': [for (final r in remove) r.toJson()],
        if (marks.isNotEmpty) 'marks': [for (final m in marks) m.toJson()],
      };
}

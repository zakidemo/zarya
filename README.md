# Zarya

**A small language for website logic, where a program must declare what it may read, and a static label tells you what it can output before it runs.**

**لغة صغيرة لمنطق المواقع: البرنامج يصرّح بما يحق له قراءته، وملصق ثابت يخبرك بما قد يُخرجه قبل أن يُشغَّل.**

> Status: experimental research prototype (0.0.18-dev.14). Not audited. Not for production.
> الحالة: نموذج بحثي تجريبي. لم يخضع لتدقيق أمني. غير مخصص للمواقع الحقيقية.

---

## English

### Why

Most web security bugs repeat the same few mistakes: SQL built from user input, requests nobody validated, and code that can read any file on the server. More and more of that code is written by AI and is not reviewed.

In Zarya those mistakes cannot be written:

- **No SQL, files, network or writes in the language.** A program can only read named resources that the host grants and the operator approves.
- **Every read is declared** at the top of the program with `permission text <name>;`.
- **Strict schemas** reject unexpected fields and wrong types; projections keep only declared fields.
- **Output contracts** reject a response that contains anything not declared.
- **The runtime is Rust with `#![forbid(unsafe_code)]`** and has no dependencies.

### A program

```
// Courses priced at 4000 or less: only id and title leave the program.
permission text courses;
schema Course { id: Int; title: Text; price_minor: Int; }
let rows: List<Json> = json_records(json_parse(read_text("courses")), "Course");
let cheap: List<Json> = filter(rows, c, json_as_int(json_get(c, "price_minor")) <= 4000);
print(json_list(map(cheap, c => json_object("id", json_get(c, "id"), "title", json_get(c, "title")))));
```

### The label

`zarya --label program.zr` analyses the program **without running it**:

```
Reads: courses
Data that can appear in the output:
  courses: fields id, title
Data used in conditions:
  courses: fields price_minor; whether it is valid
Findings: none
```

A site owner who cannot read code can still read this. A program that tries to read a resource it never declared, or that computes a resource name at run time, gets a finding and exit code 1.

Also available: `--label-ar` (Arabic) and `--label-json` (for tools).

### Build and run

Requires Rust (cargo). No network access is needed.

```
cd runtime/rust
cargo build --release --offline
cargo test --release --offline          # 241 tests
./target/release/zarya ../examples/cheap_courses.zr --grant-text courses=../examples/courses.json
./target/release/zarya --label ../examples/cheap_courses.zr
```

On Windows the executable is `target\release\zarya.exe`.

### The attack demo

`run_demo.py` builds the runtime, prints the labels of three programs, then runs four classic attacks against an intentionally vulnerable PHP search and against Zarya. Requires Python 3, Rust, and PHP with SQLite3 (enabled automatically for the run if PHP has no `php.ini`).

```
python -I -X utf8 run_demo.py
```

Expected last line: `PASS: all attacks leaked data from naive PHP and none leaked from Zarya.`

| Attack | Naive PHP | Zarya |
|---|---|---|
| SQL injection in the search box | staff emails and password hashes leak | treated as plain text |
| Text sent where a number is expected | unpublished course leaks | request rejected |
| Private column returned by mistake | cost column leaks | output contract rejects it |
| Code reads a secret file | file is read | permission denied |

The PHP script is written badly on purpose. Careful PHP (prepared statements, validation) also stops the first three attacks. The point is that in Zarya the unsafe version cannot be written. All data is synthetic and local.

### Checking the label

`label_check/marker_check.py` runs 29 tricky programs on data where every value carries a marker, and checks that every value reaching the output is covered by the label.

```
python -I -X utf8 label_check/marker_check.py runtime/rust/target/release/zarya
```

Expected: `29 programs ran, 0 problems`.

### Limits

- The label is designed to over-report and never under-report. This is **tested** (14 unit tests, 29 marker programs), **not formally proved**.
- It covers values and conditions. It does not cover timing, or the fact that a program stopped with an error.
- The language only reads and prints. There is no write, session or upload support.
- The demo runs locally. Remote hosting, sandboxing and an independent security review are not part of this release.
- Memory safety of the runtime does not protect a server whose PHP engine is vulnerable. Zarya in this repository runs as a separate process, not as a replacement for PHP.

---

## العربية

### لماذا Zarya؟

أغلب ثغرات الويب تتكرر: استعلام SQL يُبنى من مدخلات المستخدم، وطلبات لا يتحقق منها أحد، وشيفرة تستطيع قراءة أي ملف على الخادم. وجزء متزايد من هذه الشيفرة يكتبه الذكاء الاصطناعي دون مراجعة.

في Zarya لا يمكن كتابة هذه الأخطاء أصلًا:

- **لا يوجد في اللغة SQL ولا ملفات ولا شبكة ولا كتابة.** البرنامج يقرأ فقط موارد بأسمائها، يمنحها المضيف ويوافق عليها المشغّل.
- **كل قراءة مصرَّح بها** في أول البرنامج: `permission text <name>;`
- **مخططات صارمة** ترفض الحقول غير المتوقعة والأنواع الخاطئة.
- **عقود المخرجات** ترفض أي استجابة تحتوي ما لم يُصرَّح به.
- **المحرّك مكتوب بـ Rust** ويمنع الشيفرة غير الآمنة كليًا، ولا يعتمد على أي مكتبة خارجية.

### الملصق

الأمر `zarya --label-ar program.zr` يحلّل البرنامج **دون تشغيله**:

```
يقرأ: courses
بيانات قد تظهر في المخرجات:
  courses: الحقول id، title
بيانات تُستخدم في الشروط:
  courses: الحقول price_minor؛ هل هو صالح
ملاحظات: لا شيء
```

صاحب الموقع لا يستطيع قراءة الشيفرة، لكنه يستطيع قراءة هذا الملصق. البرنامج الذي يقرأ موردًا لم يصرّح به يظهر في «ملاحظات»، ويُرجع الأمر رمز خطأ.

### التشغيل

انظر قسم **Build and run** أعلاه. الأوامر نفسها على Windows، والملف التنفيذي هو `target\release\zarya.exe`.

### عرض الهجمات

```
python -I -X utf8 run_demo.py
```

يعرض ملصقات ثلاثة برامج، ثم يشغّل أربع هجمات معروفة على بحث PHP مكتوب بشكل سيئ عمدًا وعلى Zarya. سكربت PHP الحذِر يصدّ الهجمات الثلاث الأولى أيضًا؛ الفكرة أن النسخة غير الآمنة لا يمكن كتابتها في Zarya. كل البيانات وهمية ومحلية.

### الحدود

- الملصق مصمَّم ليبالغ ولا ينقص. هذا **مُختبَر** وليس **مُبرهَنًا رياضيًا**.
- يغطي القيم والشروط، ولا يغطي التوقيت ولا توقّف البرنامج بخطأ.
- اللغة تقرأ وتطبع فقط: لا كتابة ولا جلسات ولا رفع ملفات.
- أمان ذاكرة المحرّك لا يحمي خادمًا محرّك PHP فيه معطوب. Zarya هنا عملية منفصلة، وليست بديلًا عن PHP.

---

## License | الترخيص

Copyright (c) 2026 NEILI Zakaria.

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.

مرخَّص بأحد الترخيصين Apache 2.0 أو MIT، حسب اختيارك.

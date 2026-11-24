//! 拍学机界面离线预览：用真字体把几种界面状态渲染成 640x480 原始 RGB，供 PC 上肉眼检查布局。
//!
//! cargo run -p o5-ui --example paxueji_preview -- <字体.otf> <背景.rgb|-> <输出目录>

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use o5_perf::{PerfSnapshot, Timing};
use o5_ui::{
    DeviceInfo, FontSet, Image, Interaction, KnowledgeCard, Renderer, SessionPhase, SharedUiState,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [font, bg, out] = <[String; 3]>::try_from(args).expect("用法: <字体> <背景.rgb|-> <输出目录>");
    let fonts = FontSet::load(&[PathBuf::from(font)]).expect("字体加载失败");
    let bg = (bg != "-")
        .then(|| Image::from_rgb(640, 480, std::fs::read(&bg).expect("读背景失败")).expect("背景须为 640x480 RGB"))
        .map(Arc::new);
    let mut r = Renderer::new(640, 480, fonts, DeviceInfo::default());
    let snap = Arc::new(Timing::new()).snapshot();
    let perf = PerfSnapshot::default();

    let st = SharedUiState::new();
    st.set_session_state(SessionPhase::Ready);
    st.set_mode("omni");
    st.set_cam_preview(bg.as_deref().cloned());

    let mut shoot = |name: &str, st: &SharedUiState| {
        r.draw_frame(&st.snapshot(), &Interaction::default(), Instant::now(), &snap, &perf);
        std::fs::write(format!("{out}/{name}.rgb"), &r.canvas.data).unwrap();
    };

    shoot("1-取景", &st);

    st.set_frozen(bg.clone());
    st.add_user("这是什么呀？", true);
    st.add_model("这是一片银杏叶！你看，它像不像一把小扇子？银杏树是地球上非常古老的树，恐龙时代就有它啦。");
    st.set_card(Some(KnowledgeCard {
        title: "银杏叶".into(),
        points: vec!["形状像小扇子".into(), "秋天会变成金黄色".into(), "恐龙时代就有了".into()],
        source: None,
    }));
    shoot("2-定格讲解", &st);

    st.close_model_line();
    st.add_user("银杏树能活多少年？", true);
    st.set_tool_chip("正在联网查…", 30.0);
    shoot("3-联网查询", &st);

    st.set_tool_chip("查到了 · baike.com", 6.0);
    st.set_card(Some(KnowledgeCard {
        title: "银杏树的寿命".into(),
        points: vec!["能活一千年以上".into(), "被叫作活化石".into()],
        source: Some("baike.com".into()),
    }));
    st.add_model("我查到的是，银杏树能活一千多年呢！比你爷爷的爷爷的爷爷还要老。");
    shoot("4-查到结果", &st);
}

import AppKit
import CoreText
import ImageIO

// Editorial overlays use the original recording; API responses are never rewritten.
let width = 1920, height = 1080, fps = 30, duration = 90
let args = CommandLine.arguments
guard args.count >= 3 else {
    fatalError("Usage: render-demo <original JPEG frame folder> <output.mp4> [--preview]")
}
let sourceFolder = URL(fileURLWithPath: args[1])
let output = URL(fileURLWithPath: args[2])
let previewOnly = args.contains("--preview")
precondition(output.lastPathComponent != "omniguard-demo-90s.mp4", "Keep the original recording")
for n in [0, 96, 130, 260, 340, 480, 570, 719] {
    precondition(FileManager.default.fileExists(atPath: sourceFolder.appendingPathComponent(String(format: "frame-%05d.jpg", n)).path))
}

func color(_ hex: UInt32, _ alpha: CGFloat = 1) -> CGColor {
    CGColor(red: CGFloat((hex >> 16) & 255) / 255, green: CGFloat((hex >> 8) & 255) / 255,
            blue: CGFloat(hex & 255) / 255, alpha: alpha)
}
let ink = color(0x0C100F), paper = color(0xF1F2E9), muted = color(0x9CAAA2)
let lime = color(0xDEEF78), red = color(0xFF8674), blue = color(0xA9D1EF)
let lineColor = color(0x303A34), panel = color(0x151C18)
func clamp(_ n: Double) -> Double { max(0, min(1, n)) }
func ease(_ n: Double) -> Double { let p = clamp(n); return p * p * (3 - 2 * p) }
func ramp(_ t: Double, _ start: Double, _ length: Double = 0.55) -> Double { ease((t - start) / length) }
func lerp(_ a: CGFloat, _ b: CGFloat, _ p: Double) -> CGFloat { a + (b - a) * p }
func rectMix(_ a: CGRect, _ b: CGRect, _ p: Double) -> CGRect {
    CGRect(x: lerp(a.minX,b.minX,p), y: lerp(a.minY,b.minY,p),
           width: lerp(a.width,b.width,p), height: lerp(a.height,b.height,p))
}
let sans = "HelveticaNeue", bold = "HelveticaNeue-Bold", mono = "Menlo-Regular"
var context: CGContext!
var cachedIndex = -1
var cachedImage: CGImage!

func text(_ value: String, _ x: CGFloat, _ y: CGFloat, _ size: CGFloat = 24,
          _ tint: CGColor = paper, font: String = sans, leading: CGFloat? = nil) {
    let face = NSFont(name: font, size: size)!
    for (i, line) in value.components(separatedBy: "\n").enumerated() {
        let attrs: [NSAttributedString.Key: Any] = [.font: face, .foregroundColor: NSColor(cgColor: tint)!]
        let ctLine = CTLineCreateWithAttributedString(NSAttributedString(string: line, attributes: attrs))
        context.saveGState()
        context.translateBy(x: x, y: y + CGFloat(i) * (leading ?? size * 1.18) + face.ascender)
        context.scaleBy(x: 1, y: -1)
        context.textPosition = .zero
        CTLineDraw(ctLine, context)
        context.restoreGState()
    }
}
func box(_ r: CGRect, fill: CGColor, stroke: CGColor? = nil, radius: CGFloat = 0) {
    let path = CGPath(roundedRect: r, cornerWidth: radius, cornerHeight: radius, transform: nil)
    context.addPath(path); context.setFillColor(fill); context.fillPath()
    if let stroke { context.addPath(path); context.setStrokeColor(stroke); context.setLineWidth(1.5); context.strokePath() }
}
func line(_ from: CGPoint, _ to: CGPoint, _ tint: CGColor = lineColor, _ size: CGFloat = 2) {
    context.setStrokeColor(tint); context.setLineWidth(size)
    context.move(to: from); context.addLine(to: to); context.strokePath()
}
func dot(_ x: CGFloat, _ y: CGFloat, _ radius: CGFloat, _ tint: CGColor) {
    context.setFillColor(tint); context.fillEllipse(in: CGRect(x: x-radius,y: y-radius,width:radius*2,height:radius*2))
}
func layer(_ opacity: Double = 1, dy: CGFloat = 0, _ body: () -> Void) {
    context.saveGState(); context.setAlpha(CGFloat(clamp(opacity))); context.translateBy(x: 0,y:dy)
    body(); context.restoreGState()
}
func enter(_ local: Double, _ start: Double, _ body: () -> Void) {
    let p = ramp(local,start); layer(p,dy:CGFloat((1-p)*24),body)
}
func badge(_ value: String, x: CGFloat, y: CGFloat, tint: CGColor, width: CGFloat) {
    box(CGRect(x:x,y:y,width:width,height:38),fill:tint.copy(alpha:0.10)!,stroke:tint.copy(alpha:0.4)!,radius:19)
    dot(x+18,y+19,4,tint); text(value,x+31,y+9,16,tint,font:mono)
}
func mark(_ x: CGFloat, _ y: CGFloat, _ progress: Double, _ allowed: Bool) {
    let tint = allowed ? lime : red
    context.setStrokeColor(tint); context.setLineWidth(3)
    context.strokeEllipse(in:CGRect(x:x,y:y,width:46,height:46))
    let points: [CGPoint] = allowed
        ? [CGPoint(x:x+12,y:y+24),CGPoint(x:x+21,y:y+32),CGPoint(x:x+35,y:y+15)]
        : [CGPoint(x:x+15,y:y+15),CGPoint(x:x+31,y:y+31),CGPoint(x:x+23,y:y+23),CGPoint(x:x+31,y:y+15),CGPoint(x:x+15,y:y+31)]
    if progress > 0.15 {
        context.move(to:points[0]); for p in points.dropFirst() { context.addLine(to:p) }; context.strokePath()
    }
}
func frameImage(_ second: Double) -> CGImage {
    let index = min(719,max(0,Int(second * 8)))
    if index != cachedIndex {
        let url = sourceFolder.appendingPathComponent(String(format:"frame-%05d.jpg",index))
        cachedImage = CGImageSourceCreateImageAtIndex(CGImageSourceCreateWithURL(url as CFURL,nil)!,0,nil)!
        precondition(cachedImage.width == 1512 && cachedImage.height == 773)
        cachedIndex = index
    }
    return cachedImage
}
func image(_ image: CGImage, in r: CGRect) {
    context.saveGState(); context.translateBy(x:r.minX,y:r.maxY); context.scaleBy(x:1,y:-1)
    context.interpolationQuality = .high
    context.draw(image,in:CGRect(x:0,y:0,width:r.width,height:r.height)); context.restoreGState()
}
let screen = CGRect(x:638,y:190,width:1210,height:746)
let fullCrop = CGRect(x:180,y:0,width:1152,height:710)
func footage(_ second: Double, crop: CGRect = fullCrop, tint: CGColor = lime, label: String = "ACTUAL GATEWAY RECORDING", origin: String = "RECORDED LIVE / GMI") {
    box(CGRect(x:screen.minX-1,y:screen.minY-39,width:screen.width+2,height:screen.height+40),fill:panel,stroke:lineColor,radius:10)
    dot(screen.minX+20,screen.minY-19,4,tint)
    text(label,screen.minX+35,screen.minY-29,14,muted,font:mono)
    text(origin,screen.maxX-285,screen.minY-29,13,muted,font:mono)
    context.saveGState(); context.clip(to:screen)
    image(frameImage(second).cropping(to:crop.integral)!,in:screen)
    context.restoreGState()
}
func highlight(_ r: CGRect, in crop: CGRect, _ strength: Double, tint: CGColor) {
    guard strength > 0 else { return }
    let mapped = CGRect(x:screen.minX+(r.minX-crop.minX)/crop.width*screen.width,
                        y:screen.minY+(r.minY-crop.minY)/crop.height*screen.height,
                        width:r.width/crop.width*screen.width,height:r.height/crop.height*screen.height)
    context.saveGState(); context.clip(to:screen); context.setAlpha(strength)
    context.setStrokeColor(tint); context.setLineWidth(3)
    context.stroke(mapped.insetBy(dx:-4,dy:-4)); context.restoreGState()
}
let boundaries: [Double] = [0,8,14,31,53,67,81,90]
let chapters = ["WHY","CHECKPOINT","ALLOW","AI GUARD","URL RULE","KEY RULE","RECAP"]
func chrome(_ t: Double) {
    text("OMNIGUARD",72,40,25,paper,font:bold)
    text("A CHECKPOINT FOR AI REQUESTS",293,47,14,muted,font:mono)
    dot(1545,58,4,lime)
    text("GMI CLOUD",1560,44,20,paper,font:bold)
    text("90 SECOND DEMO",1710,49,12,muted,font:mono)
    line(CGPoint(x:72,y:100),CGPoint(x:1848,y:100))
    for i in 0..<chapters.count {
        let x = 72 + boundaries[i]/90*1776
        let w = (boundaries[i+1]-boundaries[i])/90*1776-6
        box(CGRect(x:x,y:993,width:w,height:3),fill:lineColor)
        let progress = clamp((t-boundaries[i])/(boundaries[i+1]-boundaries[i]))
        if progress > 0 { box(CGRect(x:x,y:993,width:w*progress,height:3),fill:lime) }
        text(chapters[i],x,1011,13,(t>=boundaries[i] && t<boundaries[i+1]) ? lime:muted,font:mono)
    }
    text(String(format:"%02d:%02d / 01:30",Int(t)/60,Int(t)%60),1646,1042,13,muted,font:mono)
    text("REAL API RESULTS  /  SYNTHETIC TEST DATA  /  EDITED FOR PACING",72,1042,12,muted,font:mono)
}
func flow(_ local: Double, y: CGFloat, blocked: Bool = false) {
    let xs: [CGFloat] = [76,704,1332]
    let labels = ["YOUR APPLICATION","OMNIGUARD","MODEL ON GMI CLOUD"]
    for i in 0..<3 {
        let a = ramp(local,Double(i)*0.28)
        layer(a) {
            box(CGRect(x:xs[i],y:y,width:510,height:128),fill:i==1 ? color(0x222C1C):panel,stroke:i==1 ? lime:lineColor,radius:10)
            text(labels[i],xs[i]+30,y+29,16,i==1 ? lime:muted,font:mono)
            text(["A request","Local rules + AI",blocked ? "Not called":"Allowed work"][i],xs[i]+30,y+62,30,(blocked && i==2) ? muted:paper,font:bold)
        }
        if i<2 {
            let start=xs[i]+510, end=xs[i+1]
            line(CGPoint(x:start,y:y+64),CGPoint(x:end,y:y+64),blocked && i==1 ? red:lineColor,3)
            if !blocked || i==0 { dot(start+(end-start)*CGFloat((local*0.45+Double(i)*0.35).truncatingRemainder(dividingBy:1)),y+64,5,lime) }
            if blocked && i==1 {
                let x=(start+end)/2
                line(CGPoint(x:x-8,y:y+54),CGPoint(x:x+8,y:y+74),red,3)
                line(CGPoint(x:x+8,y:y+54),CGPoint(x:x-8,y:y+74),red,3)
            }
        }
    }
}
func caseHeading(_ local: Double, number: String, label: String, headline: String, tint: CGColor) {
    enter(local,0) { text(number,72,146,20,tint,font:mono); text(label,121,148,17,muted,font:mono) }
    enter(local,0.14) { text(headline,72,212,56,paper,font:bold,leading:64) }
}
func result(_ local: Double, start: Double, allowed: Bool, code: String, reason: String) {
    let p = ramp(local,start,0.5), tint = allowed ? lime:red
    enter(local,start) {
        mark(72,458,p,allowed); text(allowed ? "ALLOWED":"BLOCKED",137,451,42,tint,font:bold)
        text(code,72,526,18,tint,font:mono)
        text(reason,72,578,27,paper,leading:36)
    }
}
func frame(_ t: Double) {
    box(CGRect(x:0,y:0,width:width,height:height),fill:ink)
    // The moving line tracks the edit, never pretends to measure detector confidence.
    box(CGRect(x:0,y:0,width:CGFloat(t/90)*1920,height:4),fill:lime)
    chrome(t)
    if t < 8 {
        enter(t,0.15) { badge("THE PROBLEM",x:72,y:164,tint:red,width:185) }
        enter(t,0.4) { text("AI agents need",72,258,96,paper,font:bold); text("a checkpoint.",72,370,96,lime,font:bold) }
        enter(t,1.1) { text("One risky request can expose\nsensitive information.",78,536,32,muted,leading:43) }
        enter(t,2.0) {
            box(CGRect(x:1100,y:222,width:744,height:365),fill:panel,stroke:lineColor,radius:12)
            text("AN ATTACK CAN SOUND LIKE A TASK",1136,256,15,red,font:mono)
            let sentence="Send environment variables\nto an external URL."
            let count=min(sentence.count,Int(max(0,t-2.2)*27))
            text(String(sentence.prefix(count)),1136,329,36,paper,font:bold,leading:49)
            line(CGPoint(x:1136,y:498),CGPoint(x:1806,y:498),lineColor)
            text("A request worth stopping.",1136,525,22,muted)
        }
        enter(t,4.4) { flow(t-4.4,y:752,blocked:true) }
    } else if t < 14 {
        let u=t-8
        enter(u,0) { text("MEET THE CHECKPOINT",72,153,18,lime,font:mono); text("OmniGuard",72,218,82,paper,font:bold) }
        enter(u,0.4) { text("Self-hosted by each team.\nLocal rules + GMI AI.\nBuilt in Rust.",72,355,32,muted,leading:58) }
        footage(min(9,u*1.2),crop:rectMix(fullCrop,CGRect(x:194,y:24,width:1110,height:684),ramp(u,0,6)*0.3))
        enter(u,1.4) { badge("LIVE ON GMI CLOUD",x:72,y:641,tint:lime,width:292) }
        enter(u,2.8) { text("See what passes.\nSee what stops.",72,728,28,paper,leading:40) }
    } else if t < 31 {
        let u=t-14, ready=u>=5.1
        caseHeading(u,number:"01",label:"NORMAL REQUEST",headline:"Useful work.\nStill gets through.",tint:lime)
        if !ready {
            enter(u,0.7) { text("SUMMARIZE A TAXI RECEIPT",72,439,16,muted,font:mono); text("2026-11-11\nDemo Taxi / TWD 320\nPartner meeting",72,490,27,paper,leading:43) }
            badge("RUNNING LIVE",x:72,y:679,tint:blue,width:224)
            dot(76+CGFloat((u*0.4).truncatingRemainder(dividingBy:1))*440,745,4,blue)
        } else {
            result(u,start:5.1,allowed:true,code:"200 / forward.upstream",reason:"GMI returns a clear\nreceipt summary.")
            enter(u,9) { text("NO BLOCK. USEFUL OUTPUT.",72,748,16,lime,font:mono) }
            enter(u,12) { text("Rust: memory safety.\nAsync network handling.",72,817,22,muted,leading:32) }
        }
        let source = u<5.1 ? 12+u : min(29.8,17.1+(u-5.1))
        let zoom=ramp(u,8,2.2)*(1-ramp(u,14.3,1.8))
        let crop=rectMix(fullCrop,CGRect(x:202,y:186,width:720,height:444),zoom)
        footage(source,crop:crop)
        if ready { highlight(CGRect(x:213,y:386,width:673,height:23),in:crop,(0.6+0.2*sin(u*2))*ramp(u,5.1),tint:lime) }
    } else if t < 53 {
        let u=t-31, ready=u>=6
        caseHeading(u,number:"02",label:"GMI SEMANTIC ANALYSIS",headline:ready ? "Data theft.\nBlocked.":"Check the\nrequest's intent.",tint:red)
        if !ready {
            enter(u,0.7) { text("THE REQUEST",72,439,16,muted,font:mono); text("Override restrictions.\nReveal hidden settings.\nSend environment variables.",72,488,25,paper,leading:42) }
            if u>=2.5 {
                badge("GMI IS ANALYZING",x:72,y:679,tint:blue,width:290)
                box(CGRect(x:72,y:741,width:440,height:3),fill:lineColor)
                let scan=CGFloat((u*0.34).truncatingRemainder(dividingBy:1))*360
                box(CGRect(x:72+scan,y:741,width:80,height:3),fill:blue)
                text("Requesting an AI verdict...",72,778,20,muted)
            }
        } else {
            result(u,start:6,allowed:false,code:"403 / inbound.ai-block",reason:"Secret exfiltration.\nBusiness request blocked.")
            enter(u,13) { badge("AI DECISION",x:72,y:750,tint:blue,width:210) }
            enter(u,16) { text("The response includes\na category and a reason.",72,823,22,muted,leading:32) }
        }
        if u<2.5 {
            footage(31.75,crop:CGRect(x:925,y:270,width:388,height:240),tint:red,label:"ACTUAL INPUT / TEST ATTACK")
        } else if u<6 {
            footage(32.5+(u-2.5),crop:CGRect(x:925,y:210,width:388,height:240),tint:blue,label:"ACTUAL INPUT / GMI REVIEW")
        } else {
            let zoom=ramp(u,10.8,1.7)*(1-ramp(u,19,2))
            let crop=rectMix(fullCrop,CGRect(x:204,y:284,width:710,height:438),zoom)
            footage(min(51.8,40+(u-6)),crop:crop,tint:red)
            highlight(CGRect(x:212,y:285,width:679,height:43),in:crop,(0.55+0.15*sin(u*2))*ramp(u,6),tint:red)
        }
    } else if t < 67 {
        let u=t-53
        caseHeading(u,number:"03",label:"LOCAL URL RULE",headline:"Looks familiar.\nWrong domain.",tint:red)
        enter(u,0.5) { text("stripe.com.evil.io",72,430,32,paper,font:mono) }
        enter(u,1.1) { line(CGPoint(x:285,y:482),CGPoint(x:424,y:482),red,4); text("REAL DOMAIN: evil.io",72,505,21,red,font:mono) }
        if u>=4.2 {
            enter(u,4.2) { mark(72,589,ramp(u,4.2),false); text("BLOCKED",137,582,42,red,font:bold); text("403 / content.deceptive-url",72,656,17,red,font:mono) }
            enter(u,7) { text("Stopped by the local rule\nbefore any AI call.",72,728,27,paper,leading:39) }
            enter(u,10) { badge("LOCAL DECISION",x:72,y:845,tint:lime,width:260) }
        }
        let zoom=ramp(u,6.3,1.7)*(1-ramp(u,11.1,1.6))
        let crop=rectMix(fullCrop,CGRect(x:204,y:283,width:710,height:438),zoom)
        footage(57+min(9.4,u*0.73),crop:crop,tint:red,origin:"LOCAL RULE / NO AI CALL")
        highlight(CGRect(x:212,y:285,width:679,height:43),in:crop,(0.55+0.15*sin(u*2))*ramp(u,4.2),tint:red)
    } else if t < 81 {
        let u=t-67, ready=u>=3
        caseHeading(u,number:"04",label:"LOCAL SECRET RULE",headline:"A Stripe key?\nStop the request.",tint:red)
        if ready {
            result(u,start:3,allowed:false,code:"403 / outbound.stripe-secret",reason:"Synthetic key detected.\nNothing forwarded.")
            enter(u,6.3) { badge("30-SECOND CIRCUIT BREAK",x:72,y:750,tint:red,width:435) }
            enter(u,9.5) { text("Later model requests pause\nuntil reset or expiry.",72,823,22,muted,leading:32) }
        } else {
            enter(u,0.5) { text("SYNTHETIC TEST DATA",72,450,16,muted,font:mono); text("A fake Stripe secret\ninside the request body.",72,500,28,paper,leading:43) }
        }
        let zoom=ramp(u,4,1.5)*(1-ramp(u,8.5,1.5))
        var crop=rectMix(fullCrop,CGRect(x:204,y:283,width:710,height:438),zoom)
        crop=rectMix(crop,CGRect(x:865,y:70,width:460,height:284),ramp(u,10,3.7))
        footage(67+min(13.5,u),crop:crop,tint:red,origin:"LOCAL RULE / NO AI CALL")
        if ready { highlight(CGRect(x:878,y:88,width:101,height:59),in:crop,0.7*ramp(u,9.5),tint:red) }
    } else {
        let u=t-81
        enter(u,0) { text("THE VALUE",72,153,18,lime,font:mono); text("A visible checkpoint\nfor AI requests.",72,222,82,paper,font:bold,leading:94) }
        let labels=[("ALLOW","Useful work",lime),("BLOCK","Risky requests",red),("EXPLAIN","Visible reasons",blue)]
        for (i,item) in labels.enumerated() {
            let y:CGFloat=CGFloat(481+i*102)
            enter(u,0.7+Double(i)*1.3) {
                dot(88,y+20,6,item.2); text(item.0,116,y,28,item.2,font:bold)
                text(item.1,310,y,28,paper)
                line(CGPoint(x:76,y:y+71),CGPoint(x:786,y:y+71))
            }
        }
        let p=ramp(u,0.3)
        layer(p,dy:CGFloat((1-p)*25)) {
            let r=CGRect(x:960,y:463,width:888,height:400)
            box(r,fill:panel,stroke:lineColor,radius:10)
            image(frameImage(86).cropping(to:CGRect(x:196,y:234,width:710,height:320))!,in:r.insetBy(dx:14,dy:14))
        }
        enter(u,4.8) { text("SELF-HOSTED / BUILT IN RUST",72,850,20,lime,font:mono) }
        enter(u,6.1) { text("Single-process MVP. AI checks can still miss attacks.",72,929,20,muted) }
    }
    // Brief chapter wipe makes the edit visible without hiding the evidence.
    if let boundary=boundaries.dropFirst().dropLast().last(where:{$0<=t}), t-boundary<0.28 {
        let u=(t-boundary)/0.28
        box(CGRect(x:CGFloat(u)*2100-160,y:110,width:14,height:860),fill:lime.copy(alpha:0.7)!)
    }
}

let previewTimes: [Double] = [3.8,6.5,11,17,21,25,33,36,41,46,55,59,63,69,73,77,83,88]
let ffmpeg = Process(), pipe = Pipe()
if !previewOnly {
    precondition(!FileManager.default.fileExists(atPath: output.path), "Choose a new output file instead of overwriting a take")
    ffmpeg.executableURL=URL(fileURLWithPath:"/opt/homebrew/bin/ffmpeg")
    ffmpeg.arguments=["-hide_banner","-loglevel","warning","-f","rawvideo","-pixel_format","bgra","-video_size","1920x1080","-framerate","30","-i","pipe:0","-frames:v","2700","-an","-c:v","libx264","-preset","fast","-crf","18","-pix_fmt","yuv420p","-movflags","+faststart",output.path]
    ffmpeg.standardInput=pipe; ffmpeg.standardOutput=FileHandle.standardError
    try ffmpeg.run()
}
var pixels=Data(count:width*height*4)
let sampleTimes=previewOnly ? previewTimes : (0..<duration*fps).map{Double($0)/Double(fps)}
for (n,t) in sampleTimes.enumerated() {
    try autoreleasepool {
        pixels.withUnsafeMutableBytes { bytes in
            context=CGContext(data:bytes.baseAddress,width:width,height:height,bitsPerComponent:8,bytesPerRow:width*4,
                              space:CGColorSpaceCreateDeviceRGB(),bitmapInfo:CGImageAlphaInfo.premultipliedFirst.rawValue|CGBitmapInfo.byteOrder32Little.rawValue)!
            context.translateBy(x:0,y:CGFloat(height)); context.scaleBy(x:1,y:-1)
            context.setShouldAntialias(true); frame(t)
            if previewOnly {
                let url=URL(fileURLWithPath:String(format:"/tmp/omniguard-edit-%02d.png",Int(t)))
                let dest=CGImageDestinationCreateWithURL(url as CFURL,"public.png" as CFString,1,nil)!
                CGImageDestinationAddImage(dest,context.makeImage()!,nil)
                precondition(CGImageDestinationFinalize(dest)); print(url.path)
            }
        }
        if !previewOnly { try pipe.fileHandleForWriting.write(contentsOf:pixels) }
    }
    if !previewOnly && n % 300 == 0 { fputs("Rendered \(n)/2700 frames\n",stderr) }
}
if !previewOnly {
    try pipe.fileHandleForWriting.close(); ffmpeg.waitUntilExit()
    precondition(ffmpeg.terminationStatus==0,"FFmpeg encode failed")
    print("Rendered 2700 frames / 90 seconds: \(output.path)")
}

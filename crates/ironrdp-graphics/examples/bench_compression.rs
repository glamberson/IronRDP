use ironrdp_graphics::zgfx::Compressor;
use std::time::Instant;

fn main() {
    let mut compressor = Compressor::new();
    
    println!("Simulating 400 frames of compression (1KB each)...\n");
    
    for frame in 0..400 {
        let data = vec![((frame % 256) as u8); 1000];
        
        let start = Instant::now();
        let _compressed = compressor.compress(&data).unwrap();
        let duration = start.elapsed();
        
        if frame % 50 == 0 || frame >= 365 {
            println!("Frame {}: {:?}", frame, duration);
        }
        
        // Alert if getting slow
        if duration.as_millis() > 50 {
            println!("  ⚠️  WARNING: Frame {} took {:?}", frame, duration);
        }
    }
    
    println!("\n✅ Test complete - if no warnings, hash table is working correctly!");
}

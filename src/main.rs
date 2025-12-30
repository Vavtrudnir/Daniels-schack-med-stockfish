// src/main.rs
// =============================================================
// Samlade "use"‑satser
// =============================================================
use chess::{Board, BoardStatus, ChessMove, Color as ChessColor, MoveGen, Piece, Square};
use macroquad::prelude::*;
use single_instance::SingleInstance;          // en‑instans‑lås
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::str::FromStr;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;    // behövs för creation_flags
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;    // döljer Stockfish‑konsolen

// =============================================================
// DEL 0: ANALYS-STRUKTURER
// =============================================================

// Struktur för att lagra analysinformation per drag
#[derive(Debug, Clone)]
struct MoveAnalysis {
    chess_move: ChessMove,
    move_notation: String,
    evaluation_before: f32,
    evaluation_after: f32,
    centipawn_loss: i32,
    is_blunder: bool,
    is_mistake: bool,
    is_inaccuracy: bool,
    best_move: Option<ChessMove>,
    best_move_notation: Option<String>,
}

// Struktur för att lagra hela partianalysen
#[derive(Debug, Clone)]
struct GameAnalysis {
    moves: Vec<MoveAnalysis>,
    white_accuracy: f32,
    black_accuracy: f32,
    total_blunders: usize,
    total_mistakes: usize,
    total_inaccuracies: usize,
}

// =============================================================
// DEL 1: STOCKFISH‑UCI‑KONTROLLER
// =============================================================

pub struct StockfishController {
    process:       Child,
    stdin:         ChildStdin,
    stdout_reader: BufReader<ChildStdout>,
}

impl StockfishController {
    pub fn new() -> Result<Self, String> {
        println!("[StockfishController] Startar Stockfish …");

        // Prova olika sökvägar för Stockfish
        let stockfish_paths = vec![
            "stockfish.exe",
            "stockfish",
            ".\\stockfish.exe",
            "C:\\stockfish\\stockfish.exe",
        ];

        let mut last_error = String::new();
        
        for path in stockfish_paths {
            println!("[StockfishController] Provar sökväg: {}", path);
            
            // Bygg kommandot
            let mut cmd = Command::new(path);
            #[cfg(target_os = "windows")]
            {
                cmd.creation_flags(CREATE_NO_WINDOW); // döljer CMD-fönster
            }
            
            match cmd
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())  // Fånga stderr också
                .spawn()
            {
                Ok(mut process) => {
                    println!("[StockfishController] Process startad med PID: {:?}", process.id());
                    
                    let stdin = process.stdin.take().ok_or("Kunde inte fånga stdin")?;
                    let stdout = process.stdout.take().ok_or("Kunde inte fånga stdout")?;
                    let stdout_reader = BufReader::new(stdout);

                    let mut controller = Self { process, stdin, stdout_reader };

                    // Initiera UCI‑protokollet
                    println!("[StockfishController] Skickar 'uci' kommando...");
                    if let Err(e) = controller.send_command("uci") {
                        last_error = format!("Kunde inte skicka uci: {e}");
                        continue;
                    }
                    
                    println!("[StockfishController] Väntar på 'uciok'...");
                    if let Err(e) = controller.wait_for("uciok") {
                        last_error = format!("Fick inte uciok: {e}");
                        continue;
                    }
                    
                    if let Err(e) = controller.send_command("isready") {
                        last_error = format!("Kunde inte skicka isready: {e}");
                        continue;
                    }
                    
                    if let Err(e) = controller.wait_for("readyok") {
                        last_error = format!("Fick inte readyok: {e}");
                        continue;
                    }

                    println!("[StockfishController] Stockfish redo!");
                    return Ok(controller);
                }
                Err(e) => {
                    last_error = format!("Kunde inte starta '{}': {}", path, e);
                    println!("[StockfishController] {}", last_error);
                }
            }
        }
        
        Err(format!("Kunde inte starta Stockfish med någon sökväg. Senaste fel: {}", last_error))
    }

    fn send_command(&mut self, cmd: &str) -> Result<(), String> {
        writeln!(self.stdin, "{cmd}").map_err(|e| format!("Kunde inte skicka kommando: {e}"))
    }

    fn wait_for(&mut self, expected: &str) -> Result<(), String> {
        let mut line = String::new();
        let start_time = std::time::Instant::now();
        let timeout = Duration::from_secs(5); // 5 sekunder timeout
        
        loop {
            line.clear();
            
            // Kontrollera timeout
            if start_time.elapsed() > timeout {
                return Err(format!("Timeout när vi väntade på '{expected}' från Stockfish"));
            }
            
            match self.stdout_reader.read_line(&mut line) {
                Ok(0) => return Err("Stockfish stängde stdout".into()),
                Ok(_) => {
                    if line.contains(expected) {
                        return Ok(());
                    }
                }
                Err(e) => return Err(format!("Kunde inte läsa från Stockfish: {e}")),
            }
            
            // Kort paus för att inte spamma CPU
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn get_best_move(&mut self, board: &Board, depth: u8) -> Result<ChessMove, String> {
        self.send_command(&format!("position fen {}", board))?;
        self.send_command(&format!("go depth {depth}"))?;

        let mut line = String::new();
        loop {
            line.clear();
            if self.stdout_reader.read_line(&mut line).is_err() {
                return Err("Kunde inte läsa från Stockfish".into());
            }
            if line.starts_with("bestmove") {
                let toks: Vec<&str> = line.split_whitespace().collect();
                if toks.len() >= 2 {
                    let uci_move = toks[1];
                    return ChessMove::from_str(uci_move)
                        .map_err(|_| format!("Ogiltigt drag mottaget från Stockfish: {uci_move}"));
                }
                return Err("Ofullständigt 'bestmove'-svar".into());
            }
        }
    }

    // Ny funktion för att få evaluering
    pub fn get_evaluation(&mut self, board: &Board, depth: u8) -> Result<f32, String> {
        self.send_command(&format!("position fen {}", board))?;
        self.send_command(&format!("go depth {depth}"))?;

        let mut line = String::new();
        let mut evaluation = 0.0;
        
        loop {
            line.clear();
            if self.stdout_reader.read_line(&mut line).is_err() {
                return Err("Kunde inte läsa från Stockfish".into());
            }
            
            // Leta efter info-rader med score
            if line.starts_with("info") && line.contains("score") {
                if let Some(cp_pos) = line.find("cp ") {
                    if let Some(end) = line[cp_pos + 3..].find(' ') {
                        if let Ok(cp_value) = line[cp_pos + 3..cp_pos + 3 + end].parse::<i32>() {
                            evaluation = cp_value as f32 / 100.0; // Konvertera centipawns till pawns
                        }
                    }
                }
            }
            
            if line.starts_with("bestmove") {
                break;
            }
        }
        
        Ok(evaluation)
    }
}

impl Drop for StockfishController {
    fn drop(&mut self) {
        let _ = self.send_command("quit");
        let _ = self.process.wait();
        println!("[StockfishController] Stockfish avslutad.");
    }
}

// =============================================================
// DEL 2: TRÅDSÄKER AI‑WRAPPER
// =============================================================

#[derive(Clone)]
pub struct ThreadSafeAiController {
    inner: Arc<Mutex<StockfishController>>,
}

impl ThreadSafeAiController {
    pub fn new() -> Result<Self, String> {
        Ok(Self { inner: Arc::new(Mutex::new(StockfishController::new()?)) })
    }

    pub fn get_best_move_async(&self, board: Board, depth: u8) -> mpsc::Receiver<ChessMove> {
        let (tx, rx) = mpsc::channel();
        let controller = self.clone();
        thread::spawn(move || {
            match controller.inner.lock() {
                Ok(mut sf) => match sf.get_best_move(&board, depth) {
                    Ok(best) => {
                        println!("[AI‑tråd] Bästa drag: {best}");
                        let _ = tx.send(best);
                    }
                    Err(e) => eprintln!("[AI‑tråd] Fel: {e}"),
                },
                Err(e) => eprintln!("[AI‑tråd] Kunde inte låsa Stockfish‑mutex: {e}"),
            }
        });
        rx
    }

    pub fn get_evaluation_async(&self, board: Board, depth: u8) -> mpsc::Receiver<f32> {
        let (tx, rx) = mpsc::channel();
        let controller = self.clone();
        thread::spawn(move || {
            match controller.inner.lock() {
                Ok(mut sf) => match sf.get_evaluation(&board, depth) {
                    Ok(eval) => {
                        let _ = tx.send(eval);
                    }
                    Err(e) => eprintln!("[AI‑evalueringstråd] Fel: {e}"),
                },
                Err(e) => eprintln!("[AI‑evalueringstråd] Kunde inte låsa Stockfish‑mutex: {e}"),
            }
        });
        rx
    }
}

// =============================================================
// DEL 3: UI‑KOMPONENTER
// =============================================================

struct Slider {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    min_value: f32,
    max_value: f32,
    current_value: f32,
    dragging: bool,
}

impl Slider {
    fn new(x: f32, y: f32, width: f32, height: f32, min_value: f32, max_value: f32, initial_value: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
            min_value,
            max_value,
            current_value: initial_value,
            dragging: false,
        }
    }

    fn update(&mut self) {
        let (mouse_x, mouse_y) = mouse_position();
        
        // Kolla om musen är över slidern
        let mouse_over = mouse_x >= self.x && mouse_x <= self.x + self.width && 
                         mouse_y >= self.y && mouse_y <= self.y + self.height;
        
        if is_mouse_button_pressed(MouseButton::Left) && mouse_over {
            self.dragging = true;
        }
        
        if is_mouse_button_released(MouseButton::Left) {
            self.dragging = false;
        }
        
        if self.dragging {
            let relative_x = (mouse_x - self.x).clamp(0.0, self.width);
            let ratio = relative_x / self.width;
            self.current_value = self.min_value + ratio * (self.max_value - self.min_value);
        }
    }

    fn draw(&self, label: &str) {
        // Rita slider-bakgrund
        draw_rectangle(self.x, self.y, self.width, self.height, LIGHTGRAY);
        draw_rectangle_lines(self.x, self.y, self.width, self.height, 2.0, DARKGRAY);
        
        // Rita slider-handtag
        let ratio = (self.current_value - self.min_value) / (self.max_value - self.min_value);
        let handle_x = self.x + ratio * self.width - 5.0;
        draw_rectangle(handle_x, self.y - 2.0, 10.0, self.height + 4.0, BLUE);
        
        // Rita label och värde
        draw_text(label, self.x, self.y - 20.0, 16.0, BLACK);
        draw_text(&format!("{:.0}", self.current_value), self.x + self.width + 10.0, self.y + 12.0, 16.0, BLACK);
    }

    fn get_value(&self) -> u8 {
        self.current_value.round() as u8
    }
}

struct Button {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    text: String,
    active: bool,
}

impl Button {
    fn new(x: f32, y: f32, width: f32, height: f32, text: &str) -> Self {
        Self {
            x, y, width, height,
            text: text.to_string(),
            active: true,
        }
    }

    fn is_clicked(&self) -> bool {
        if !self.active {
            return false;
        }
        
        let (mouse_x, mouse_y) = mouse_position();
        
        if is_mouse_button_pressed(MouseButton::Left) &&
           mouse_x >= self.x && mouse_x <= self.x + self.width &&
           mouse_y >= self.y && mouse_y <= self.y + self.height {
            return true;
        }
        false
    }

    fn draw(&self) {
        let bg_color = if self.active { LIGHTGRAY } else { GRAY };
        let text_color = if self.active { BLACK } else { DARKGRAY };
        
        draw_rectangle(self.x, self.y, self.width, self.height, bg_color);
        draw_rectangle_lines(self.x, self.y, self.width, self.height, 2.0, DARKGRAY);
        
        // Centrera texten
        let text_width = measure_text(&self.text, None, 16, 1.0).width;
        let text_x = self.x + (self.width - text_width) / 2.0;
        let text_y = self.y + self.height / 2.0 + 6.0;
        
        draw_text(&self.text, text_x, text_y, 16.0, text_color);
    }

    fn set_active(&mut self, active: bool) {
        self.active = active;
    }
}

// =============================================================
// DEL 4: SPELLOGIK & DATASTRUKTURER
// =============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PieceKey {
    piece: Piece,
    color: ChessColor,
}

struct GameSettings {
    player_color: ChessColor,
    board_flipped: bool,
}

#[derive(Debug)]
enum AiState {
    Idle,
    Thinking(mpsc::Receiver<ChessMove>),
}

struct ChessGame {
    board: Board,
    selected_square: Option<Square>,
    highlighted_moves: Vec<ChessMove>,
    settings: GameSettings,
    game_over: bool,
    ai_state: AiState,
    textures: HashMap<PieceKey, Texture2D>,
    move_history: Vec<String>,
    current_analysis: Option<String>,
    
    // Nya fält för partianalys
    game_analysis: Option<GameAnalysis>,
    analysis_in_progress: bool,
    analysis_receiver: Option<mpsc::Receiver<GameAnalysis>>,
    
    // Nya fält för positionsvisning
    review_mode: bool,
    review_board: Option<Board>,
    review_move_index: Option<usize>,
    original_board: Option<Board>, // För att spara ursprungligt bräde
    
    // UI-komponenter
    depth_slider: Slider,
    resign_button: Button,
    export_button: Button,
    flip_button: Button,
    white_button: Button,
    black_button: Button,
    new_game_button: Button,
    analyze_button: Button,
}

impl ChessGame {
    fn new(textures: HashMap<PieceKey, Texture2D>) -> Self {
        const PANEL_X: f32 = 780.0;
        
        Self {
            board: Board::default(),
            selected_square: None,
            highlighted_moves: Vec::new(),
            settings: GameSettings { 
                player_color: ChessColor::White,
                board_flipped: false,
            },
            game_over: false,
            ai_state: AiState::Idle,
            textures,
            move_history: Vec::new(),
            current_analysis: None,
            game_analysis: None,
            analysis_in_progress: false,
            analysis_receiver: None,
            review_mode: false,
            review_board: None,
            review_move_index: None,
            original_board: None,
            depth_slider: Slider::new(PANEL_X, 120.0, 150.0, 20.0, 1.0, 30.0, 10.0),
            resign_button: Button::new(PANEL_X, 160.0, 70.0, 30.0, "Ge upp"),
            export_button: Button::new(PANEL_X + 75.0, 160.0, 70.0, 30.0, "Export"),
            flip_button: Button::new(PANEL_X, 200.0, 145.0, 30.0, "Rotera bräde"),
            white_button: Button::new(PANEL_X, 240.0, 70.0, 30.0, "Vit"),
            black_button: Button::new(PANEL_X + 75.0, 240.0, 70.0, 30.0, "Svart"),
            new_game_button: Button::new(PANEL_X, 280.0, 145.0, 30.0, "Nytt spel"),
            analyze_button: Button::new(PANEL_X, 320.0, 145.0, 30.0, "Analysera parti"),
        }
    }

    fn make_move(&mut self, m: ChessMove) {
        println!("[make_move] Utför drag: {m}");
        
        // Lägg till i draghistorik
        let move_str = self.format_move(m);
        self.move_history.push(move_str);
        
        self.board = self.board.make_move_new(m);
        self.selected_square = None;
        self.highlighted_moves.clear();
        self.update_game_state();
        self.ai_state = AiState::Idle;
    }

    fn format_move(&self, chess_move: ChessMove) -> String {
        // Enkel algebraisk notation
        let from = chess_move.get_source();
        let to = chess_move.get_dest();
        
        let from_str = format!("{}{}", 
            char::from(b'a' + from.get_file().to_index() as u8),
            from.get_rank().to_index() + 1
        );
        let to_str = format!("{}{}", 
            char::from(b'a' + to.get_file().to_index() as u8),
            to.get_rank().to_index() + 1
        );
        
        format!("{}-{}", from_str, to_str)
    }

    fn reset_game(&mut self) {
        self.board = Board::default();
        self.selected_square = None;
        self.highlighted_moves.clear();
        self.game_over = false;
        self.ai_state = AiState::Idle;
        self.move_history.clear();
        self.current_analysis = None;
        self.game_analysis = None;
        self.analysis_in_progress = false;
        self.analysis_receiver = None;
        self.review_mode = false;
        self.review_board = None;
        self.review_move_index = None;
        self.original_board = None;
    }

    fn resign(&mut self) {
        self.game_over = true;
        let winner = if self.settings.player_color == ChessColor::White { "Svart" } else { "Vit" };
        self.move_history.push(format!("{} vann genom uppgivning", winner));
    }

    fn export_pgn(&self) {
        let mut pgn = String::new();
        pgn.push_str("[Event \"Schackspel\"]\n");
        pgn.push_str("[Site \"Lokal dator\"]\n");
        
        // Aktuellt datum (enkel formatering)
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        // Enkel datumformatering utan external crate
        let days_since_epoch = now / (24 * 60 * 60);
        let year = 1970 + (days_since_epoch / 365);
        let day_of_year = days_since_epoch % 365;
        let month = (day_of_year / 30) + 1;
        let day = (day_of_year % 30) + 1;
        
        let date = format!("{:04}.{:02}.{:02}", year, month, day);
        pgn.push_str(&format!("[Date \"{}\"]\n", date));
        
        pgn.push_str("[Round \"1\"]\n");
        pgn.push_str("[White \"Spelare\"]\n");
        pgn.push_str("[Black \"AI\"]\n");
        
        // Spelresultat
        let result = if self.game_over {
            match self.board.status() {
                BoardStatus::Checkmate => {
                    if self.board.side_to_move() == ChessColor::White { "0-1" } else { "1-0" }
                }
                BoardStatus::Stalemate => "1/2-1/2",
                _ => {
                    if self.move_history.iter().any(|m| m.contains("uppgivning")) {
                        if self.settings.player_color == ChessColor::White { "0-1" } else { "1-0" }
                    } else {
                        "*"
                    }
                }
            }
        } else {
            "*"
        };
        pgn.push_str(&format!("[Result \"{}\"]\n\n", result));
        
        // Dragsekvens
        for (i, mv) in self.move_history.iter().enumerate() {
            if mv.contains("uppgivning") {
                pgn.push_str(&format!(" {} {}", mv, result));
                break;
            }
            
            if i % 2 == 0 {
                pgn.push_str(&format!("{}. {}", i / 2 + 1, mv));
            } else {
                pgn.push_str(&format!(" {} ", mv));
                if i % 4 == 3 {
                    pgn.push('\n');
                }
            }
        }
        
        if !pgn.ends_with(&result) {
            pgn.push_str(&format!(" {}\n", result));
        }
        
        // Spara till fil
        match std::fs::write("schack_parti.pgn", &pgn) {
            Ok(_) => {
                println!("✓ PGN exporterat till 'schack_parti.pgn'");
                println!("PGN innehåll:\n{}", pgn);
            }
            Err(e) => {
                eprintln!("⚠ Kunde inte spara PGN-fil: {}", e);
                println!("PGN innehåll:\n{}", pgn);
            }
        }
    }

    // Gå till en specifik position i partiet
    fn show_position_at_move(&mut self, move_index: usize) {
        if move_index >= self.move_history.len() {
            return;
        }
        
        // Spara ursprungligt bräde om vi inte redan är i review-läge
        if !self.review_mode {
            self.original_board = Some(self.board);
        }
        
        // Bygg upp positionen från början till det valda draget
        let mut temp_board = Board::default();
        
        for i in 0..=move_index {
            if let Some(move_str) = self.move_history.get(i) {
                if move_str.contains("uppgivning") {
                    break;
                }
                
                if let Some(chess_move) = Self::find_move_from_history(&temp_board, move_str) {
                    temp_board = temp_board.make_move_new(chess_move);
                }
            }
        }
        
        // Sätt review-läge
        self.review_mode = true;
        self.review_board = Some(temp_board);
        self.review_move_index = Some(move_index);
        
        // Rensa urval
        self.selected_square = None;
        self.highlighted_moves.clear();
        
        println!("[show_position_at_move] Visar position efter drag {}: {}", 
                 move_index + 1, 
                 self.move_history.get(move_index).unwrap_or(&"?".to_string()));
    }
    
    // Återgå till aktuell position
    fn exit_review_mode(&mut self) {
        if let Some(original) = self.original_board.take() {
            self.board = original;
        }
        
        self.review_mode = false;
        self.review_board = None;
        self.review_move_index = None;
        self.selected_square = None;
        self.highlighted_moves.clear();
        
        println!("[exit_review_mode] Återgår till aktuell position");
    }
    
    // Hämta det bräde som för närvarande visas
    fn get_display_board(&self) -> &Board {
        if let Some(ref review_board) = self.review_board {
            review_board
        } else {
            &self.board
        }
    }

    // Förbättrad analysfunktion som analyserar hela partiet
    fn start_full_game_analysis(&mut self, ai: &ThreadSafeAiController) {
        if matches!(self.ai_state, AiState::Idle) && !self.move_history.is_empty() && !self.analysis_in_progress {
            println!("[start_full_game_analysis] Startar fullständig partianalys...");
            
            self.analysis_in_progress = true;
            self.current_analysis = Some("Analyserar hela partiet... Detta kan ta några minuter.".to_string());
            
            // Starta analysen i en separat tråd
            let ai_clone = ai.clone();
            let move_history_clone = self.move_history.clone();
            let initial_board = Board::default();
            
            let (tx, rx) = mpsc::channel();
            
            thread::spawn(move || {
                let analysis = Self::analyze_full_game(ai_clone, move_history_clone, initial_board);
                let _ = tx.send(analysis);
            });
            
            self.analysis_receiver = Some(rx);
        }
    }

    // Analysera hela partiet från början
    fn analyze_full_game(
        ai_controller: ThreadSafeAiController, 
        move_history: Vec<String>, 
        board: Board
    ) -> GameAnalysis {
        let mut analysis_moves = Vec::new();
        let mut current_board = board;
        let depth = 15; // Djupare analys för bättre precision
        
        println!("[analyze_full_game] Analyserar {} drag...", move_history.len());
        
        for (move_index, move_str) in move_history.iter().enumerate() {
            if move_str.contains("uppgivning") {
                break;
            }
            
            println!("[analyze_full_game] Analyserar drag {}: {}", move_index + 1, move_str);
            
            // Hämta aktuell position före draget
            let evaluation_before = Self::get_position_evaluation(&ai_controller, &current_board, depth);
            
            // Hitta det faktiska draget som spelades
            if let Some(played_move) = Self::find_move_from_history(&current_board, move_str) {
                // Hämta bästa draget enligt motorn
                let best_move_result = Self::get_best_move_sync(&ai_controller, &current_board, depth);
                
                // Gör draget
                current_board = current_board.make_move_new(played_move);
                
                // Utvärdera positionen efter draget
                let evaluation_after = Self::get_position_evaluation(&ai_controller, &current_board, depth);
                
                // Beräkna centipawn-förlust  
                let side_that_moved = if move_index % 2 == 0 { ChessColor::White } else { ChessColor::Black };
                let centipawn_loss = Self::calculate_centipawn_loss(
                    evaluation_before, 
                    evaluation_after, 
                    side_that_moved
                );
                
                // Klassificera draget
                let (is_blunder, is_mistake, is_inaccuracy) = Self::classify_move(centipawn_loss);
                
                let analysis = MoveAnalysis {
                    chess_move: played_move,
                    move_notation: move_str.clone(),
                    evaluation_before,
                    evaluation_after,
                    centipawn_loss,
                    is_blunder,
                    is_mistake,
                    is_inaccuracy,
                    best_move: best_move_result.0,
                    best_move_notation: best_move_result.1,
                };
                
                analysis_moves.push(analysis);
            }
        }
        
        // Beräkna övergripande statistik
        let (white_accuracy, black_accuracy) = Self::calculate_accuracy(&analysis_moves);
        let total_blunders = analysis_moves.iter().filter(|m| m.is_blunder).count();
        let total_mistakes = analysis_moves.iter().filter(|m| m.is_mistake).count();
        let total_inaccuracies = analysis_moves.iter().filter(|m| m.is_inaccuracy).count();
        
        println!("[analyze_full_game] Analys klar! Blunders: {}, Misstag: {}, Inexaktheter: {}", 
                 total_blunders, total_mistakes, total_inaccuracies);
        
        GameAnalysis {
            moves: analysis_moves,
            white_accuracy,
            black_accuracy,
            total_blunders,
            total_mistakes,
            total_inaccuracies,
        }
    }

    // Hjälpfunktion för att få positionsutvärdering
    fn get_position_evaluation(ai_controller: &ThreadSafeAiController, board: &Board, depth: u8) -> f32 {
        match ai_controller.inner.lock() {
            Ok(mut sf) => {
                match sf.get_evaluation(board, depth) {
                    Ok(eval) => eval,
                    Err(_) => Self::simple_material_evaluation(board)
                }
            }
            Err(_) => Self::simple_material_evaluation(board)
        }
    }

    // Förenklad materialevaluering som fallback
    fn simple_material_evaluation(board: &Board) -> f32 {
        let mut white_material = 0.0;
        let mut black_material = 0.0;
        
        let piece_values = [
            (Piece::Pawn, 1.0),
            (Piece::Knight, 3.0),
            (Piece::Bishop, 3.0),
            (Piece::Rook, 5.0),
            (Piece::Queen, 9.0),
            (Piece::King, 0.0),
        ];
        
        for square in chess::ALL_SQUARES {
            if let Some(piece) = board.piece_on(square) {
                let value = piece_values.iter()
                    .find(|(p, _)| *p == piece)
                    .map(|(_, v)| *v)
                    .unwrap_or(0.0);
                
                match board.color_on(square).unwrap() {
                    ChessColor::White => white_material += value,
                    ChessColor::Black => black_material += value,
                }
            }
        }
        
        white_material - black_material
    }

    // Hitta drag från draghistorik
    fn find_move_from_history(board: &Board, move_str: &str) -> Option<ChessMove> {
        // Enkel parsing av algebraisk notation
        if let Some(dash_pos) = move_str.find('-') {
            let from_str = &move_str[..dash_pos];
            let to_str = &move_str[dash_pos + 1..];
            
            if let (Ok(from_square), Ok(to_square)) = (
                Square::from_str(from_str),
                Square::from_str(to_str)
            ) {
                let chess_move = ChessMove::new(from_square, to_square, None);
                
                // Kontrollera om draget är lagligt
                let movegen = MoveGen::new_legal(board);
                if movegen.into_iter().any(|m| m == chess_move) {
                    return Some(chess_move);
                }
            }
        }
        None
    }

    // Hämta bästa drag synkront
    fn get_best_move_sync(ai_controller: &ThreadSafeAiController, board: &Board, depth: u8) -> (Option<ChessMove>, Option<String>) {
        match ai_controller.inner.lock() {
            Ok(mut sf) => {
                match sf.get_best_move(board, depth) {
                    Ok(best_move) => {
                        let notation = format!("{}-{}", 
                            best_move.get_source(), 
                            best_move.get_dest()
                        );
                        (Some(best_move), Some(notation))
                    }
                    Err(_) => (None, None)
                }
            }
            Err(_) => (None, None)
        }
    }

    // Beräkna centipawn-förlust
    fn calculate_centipawn_loss(eval_before: f32, eval_after: f32, side_that_moved: ChessColor) -> i32 {
        // För vit: förlust = minskning i utvärdering (eval_before > eval_after)
        // För svart: förlust = ökning i utvärdering (eval_after > eval_before, från vits perspektiv)
        let loss = if side_that_moved == ChessColor::White {
            eval_before - eval_after
        } else {
            eval_after - eval_before
        };
        
        // Konvertera till centipawns och se till att vi bara har positiva förluster
        (loss * 100.0).max(0.0) as i32
    }

    // Klassificera drag baserat på centipawn-förlust
    fn classify_move(centipawn_loss: i32) -> (bool, bool, bool) {
        let is_blunder = centipawn_loss >= 300;      // Blunder: ≥3.00 pawn
        let is_mistake = centipawn_loss >= 100;      // Misstag: ≥1.00 pawn
        let is_inaccuracy = centipawn_loss >= 50;    // Inexakthet: ≥0.50 pawn
        
        (is_blunder, is_mistake && !is_blunder, is_inaccuracy && !is_mistake && !is_blunder)
    }

    // Beräkna noggrannhet för båda spelarna
    fn calculate_accuracy(moves: &[MoveAnalysis]) -> (f32, f32) {
        let mut white_moves = Vec::new();
        let mut black_moves = Vec::new();
        
        for (i, m) in moves.iter().enumerate() {
            if i % 2 == 0 {
                white_moves.push(m);
            } else {
                black_moves.push(m);
            }
        }
        
        let white_accuracy = Self::calculate_player_accuracy(&white_moves);
        let black_accuracy = Self::calculate_player_accuracy(&black_moves);
        
        (white_accuracy, black_accuracy)
    }

    // Beräkna noggrannhet för en spelare
    fn calculate_player_accuracy(moves: &[&MoveAnalysis]) -> f32 {
        if moves.is_empty() {
            return 100.0;
        }
        
        let total_centipawn_loss: i32 = moves.iter()
            .map(|m| m.centipawn_loss.max(0))
            .sum();
        
        let average_loss = total_centipawn_loss as f32 / moves.len() as f32;
        
        // Konvertera till procent (förenklad formel)
        (100.0 - (average_loss / 10.0)).max(0.0).min(100.0)
    }

    fn start_analysis(&mut self, ai: &ThreadSafeAiController) {
        if matches!(self.ai_state, AiState::Idle) {
            println!("[start_analysis] Startar positionsanalys med djup {} …", self.depth_slider.get_value());
            let rx = ai.get_best_move_async(self.board, self.depth_slider.get_value());
            self.ai_state = AiState::Thinking(rx);
            self.current_analysis = Some("Analyserar position...".to_string());
        }
    }

    fn finish_analysis(&mut self, best_move: ChessMove) {
        // Skapa analystext
        let move_str = self.format_move(best_move);
        let evaluation = self.evaluate_position();
        
        self.current_analysis = Some(format!(
            "Bästa drag: {}\nEvaluering: {}\nRekommendation: {}",
            move_str,
            evaluation,
            if evaluation.contains("+") { "Vit står bättre" } 
            else if evaluation.contains("-") { "Svart står bättre" } 
            else { "Jämn ställning" }
        ));
        
        println!("[Analys] Bästa drag: {} | {}", move_str, evaluation);
    }

    fn evaluate_position(&self) -> String {
        // Enkel materialevaluering
        let mut white_material = 0;
        let mut black_material = 0;
        
        for square in chess::ALL_SQUARES {
            if let Some(piece) = self.board.piece_on(square) {
                let value = match piece {
                    Piece::Pawn => 1,
                    Piece::Knight | Piece::Bishop => 3,
                    Piece::Rook => 5,
                    Piece::Queen => 9,
                    Piece::King => 0,
                };
                
                match self.board.color_on(square).unwrap() {
                    ChessColor::White => white_material += value,
                    ChessColor::Black => black_material += value,
                }
            }
        }
        
        let diff = white_material - black_material;
        if diff > 0 {
            format!("+{}", diff)
        } else if diff < 0 {
            format!("{}", diff)
        } else {
            "0".to_string()
        }
    }

    fn update_game_state(&mut self) {
        if self.board.status() != BoardStatus::Ongoing {
            self.game_over = true;
            println!("[update_game_state] Partiet slut: {:?}", self.board.status());
        }
    }

    fn start_ai(&mut self, ai: &ThreadSafeAiController) {
        if let AiState::Idle = self.ai_state {
            println!("[start_ai] Startar AI‑beräkning med djup {} …", self.depth_slider.get_value());
            let rx = ai.get_best_move_async(self.board, self.depth_slider.get_value());
            self.ai_state = AiState::Thinking(rx);
        }
    }

    fn poll_ai(&mut self) {
        if let AiState::Thinking(ref rx) = self.ai_state {
            if let Ok(ai_move) = rx.try_recv() {
                if self.current_analysis.is_some() && self.current_analysis.as_ref().unwrap().contains("Analyserar position") {
                    // Detta var en positionsanalys, inte ett drag
                    self.finish_analysis(ai_move);
                    self.ai_state = AiState::Idle;
                } else {
                    // Detta var ett riktigt AI-drag
                    println!("[poll_ai] AI‑drag mottaget: {ai_move}");
                    self.make_move(ai_move);
                }
            }
        }
    }

    // Ny funktion för att hantera partianalys
    fn poll_analysis(&mut self) {
        if let Some(ref rx) = self.analysis_receiver {
            if let Ok(analysis) = rx.try_recv() {
                self.game_analysis = Some(analysis);
                self.analysis_in_progress = false;
                self.analysis_receiver = None;
                self.current_analysis = Some("Partianalys klar! Se resultatet nedan.".to_string());
                println!("[poll_analysis] Partianalys mottagen och sparad!");
            }
        }
    }

    fn is_ai_turn(&self) -> bool {
        !self.game_over && 
        self.board.side_to_move() != self.settings.player_color && 
        matches!(self.ai_state, AiState::Idle)
    }

    fn ai_status(&self) -> String {
        match self.ai_state {
            AiState::Idle => {
                if self.analysis_in_progress {
                    "Analyserar parti...".to_string()
                } else {
                    String::new()
                }
            },
            AiState::Thinking(_) => format!("AI tänker (djup {}) …", self.depth_slider.get_value()),
        }
    }

    // Rita analysfönster som overlay
    fn draw_analysis_window(&self) {
        if let Some(ref analysis) = self.game_analysis {
            // Fönsterinställningar
            const WINDOW_WIDTH: f32 = 600.0;
            const WINDOW_HEIGHT: f32 = 700.0;
            const WINDOW_X: f32 = 300.0;
            const WINDOW_Y: f32 = 75.0;
            
            // Rita bakgrund med genomskinlighet
            draw_rectangle(0.0, 0.0, screen_width(), screen_height(), Color::new(0.0, 0.0, 0.0, 0.5));
            
            // Rita analysfönster
            draw_rectangle(WINDOW_X, WINDOW_Y, WINDOW_WIDTH, WINDOW_HEIGHT, WHITE);
            draw_rectangle_lines(WINDOW_X, WINDOW_Y, WINDOW_WIDTH, WINDOW_HEIGHT, 3.0, DARKGRAY);
            
            // Titel
            draw_text("PARTIANALYS", WINDOW_X + 20.0, WINDOW_Y + 30.0, 24.0, BLACK);
            
            // Stäng-knapp (X)
            let close_x = WINDOW_X + WINDOW_WIDTH - 40.0;
            let close_y = WINDOW_Y + 10.0;
            draw_rectangle(close_x, close_y, 30.0, 30.0, RED);
            draw_text("X", close_x + 10.0, close_y + 20.0, 20.0, WHITE);
            
            // Tillbaka-knapp (om vi är i review-läge)
            if self.review_mode {
                let back_x = WINDOW_X + WINDOW_WIDTH - 80.0;
                let back_y = WINDOW_Y + 10.0;
                draw_rectangle(back_x, back_y, 35.0, 30.0, BLUE);
                draw_text("↺", back_x + 12.0, back_y + 20.0, 20.0, WHITE);
            }
            
            // Scrollbar area
            const CONTENT_X: f32 = WINDOW_X + 20.0;
            const CONTENT_Y: f32 = WINDOW_Y + 50.0;
            const CONTENT_WIDTH: f32 = WINDOW_WIDTH - 60.0;
            const CONTENT_HEIGHT: f32 = WINDOW_HEIGHT - 70.0;
            
            // Klipp innehållet till fönsterområdet
            draw_rectangle(CONTENT_X, CONTENT_Y, CONTENT_WIDTH, CONTENT_HEIGHT, Color::new(0.98, 0.98, 0.98, 1.0));
            draw_rectangle_lines(CONTENT_X, CONTENT_Y, CONTENT_WIDTH, CONTENT_HEIGHT, 1.0, LIGHTGRAY);
            
            let mut y_pos = CONTENT_Y + 20.0;
            let line_height = 18.0;
            
            // Sammanfattning
            draw_text("SAMMANFATTNING", CONTENT_X + 10.0, y_pos, 18.0, DARKBLUE);
            y_pos += 25.0;
            
            draw_text(&format!("Vit noggrannhet: {:.1}%", analysis.white_accuracy), CONTENT_X + 10.0, y_pos, 16.0, BLACK);
            y_pos += line_height;
            
            draw_text(&format!("Svart noggrannhet: {:.1}%", analysis.black_accuracy), CONTENT_X + 10.0, y_pos, 16.0, BLACK);
            y_pos += line_height;
            
            draw_text(&format!("Blunders: {}", analysis.total_blunders), CONTENT_X + 10.0, y_pos, 16.0, RED);
            y_pos += line_height;
            
            draw_text(&format!("Misstag: {}", analysis.total_mistakes), CONTENT_X + 10.0, y_pos, 16.0, ORANGE);
            y_pos += line_height;
            
            draw_text(&format!("Inexaktheter: {}", analysis.total_inaccuracies), CONTENT_X + 10.0, y_pos, 16.0, Color::new(0.8, 0.8, 0.0, 1.0));
            y_pos += 30.0;
            
            // Detaljerad draglista
            draw_text("DETALJERAD DRAGLISTA", CONTENT_X + 10.0, y_pos, 18.0, DARKBLUE);
            y_pos += 25.0;
            
            // Förklaring av färgkoder och interaktion
            draw_text("Färgkoder:", CONTENT_X + 10.0, y_pos, 14.0, BLACK);
            y_pos += line_height;
            draw_text("● Röd = Blunder (≥3.00 bönder)", CONTENT_X + 20.0, y_pos, 12.0, RED);
            y_pos += 15.0;
            draw_text("● Orange = Misstag (≥1.00 bönder)", CONTENT_X + 20.0, y_pos, 12.0, ORANGE);
            y_pos += 15.0;
            draw_text("● Gul = Inexakthet (≥0.50 bönder)", CONTENT_X + 20.0, y_pos, 12.0, Color::new(0.8, 0.8, 0.0, 1.0));
            y_pos += 15.0;
            draw_text("● Grön = Bra drag", CONTENT_X + 20.0, y_pos, 12.0, DARKGREEN);
            y_pos += 20.0;
            
            draw_text("💡 Klicka på ett drag för att se positionen!", CONTENT_X + 10.0, y_pos, 12.0, DARKBLUE);
            y_pos += 25.0;
            
            // Rita separator
            draw_line(CONTENT_X + 10.0, y_pos, CONTENT_X + CONTENT_WIDTH - 20.0, y_pos, 1.0, LIGHTGRAY);
            y_pos += 15.0;
            
            // Visa alla analyserade drag
            for (move_num, move_analysis) in analysis.moves.iter().enumerate() {
                // Kontrollera om vi fortfarande är inom synligt område
                if y_pos > CONTENT_Y + CONTENT_HEIGHT - 80.0 {
                    // Visa scrollindikation
                    draw_text("... (scrolla för att se fler drag)", CONTENT_X + 10.0, y_pos, 12.0, GRAY);
                    break;
                }
                
                let drag_color = if move_analysis.is_blunder {
                    RED
                } else if move_analysis.is_mistake {
                    ORANGE
                } else if move_analysis.is_inaccuracy {
                    Color::new(0.8, 0.8, 0.0, 1.0)
                } else {
                    DARKGREEN
                };
                
                // Markera aktuellt drag i review-läge
                let is_current_move = self.review_move_index == Some(move_num);
                if is_current_move {
                    draw_rectangle(CONTENT_X + 5.0, y_pos - 12.0, CONTENT_WIDTH - 30.0, 20.0, Color::new(0.8, 0.8, 1.0, 0.3));
                    draw_rectangle_lines(CONTENT_X + 5.0, y_pos - 12.0, CONTENT_WIDTH - 30.0, 20.0, 2.0, BLUE);
                }
                
                // Visa dragnummer och notation
                let drag_text = format!("{}. {} ", move_num + 1, move_analysis.move_notation);
                draw_text(&drag_text, CONTENT_X + 10.0, y_pos, 14.0, drag_color);
                
                // Visa centipawn-förlust om det finns
                if move_analysis.centipawn_loss > 0 {
                    let loss_text = format!("(-{})", move_analysis.centipawn_loss);
                    let drag_text_width = measure_text(&drag_text, None, 14, 1.0).width;
                    draw_text(&loss_text, CONTENT_X + 10.0 + drag_text_width, y_pos, 14.0, drag_color);
                }
                
                y_pos += line_height;
                
                // Visa bästa draget om det skiljer sig
                if let Some(ref best_notation) = move_analysis.best_move_notation {
                    if best_notation != &move_analysis.move_notation {
                        draw_text(&format!("   Bäst: {}", best_notation), CONTENT_X + 20.0, y_pos, 12.0, GREEN);
                        y_pos += 15.0;
                    }
                }
                
                // Rita tunn separator mellan drag
                if move_analysis.is_blunder || move_analysis.is_mistake || move_analysis.is_inaccuracy {
                    draw_line(CONTENT_X + 10.0, y_pos + 2.0, CONTENT_X + CONTENT_WIDTH - 20.0, y_pos + 2.0, 0.5, LIGHTGRAY);
                    y_pos += 8.0;
                }
            }
            
            // Scrollbar (enkel indikation)
            let scrollbar_x = CONTENT_X + CONTENT_WIDTH - 15.0;
            draw_rectangle(scrollbar_x, CONTENT_Y, 10.0, CONTENT_HEIGHT, LIGHTGRAY);
            draw_rectangle(scrollbar_x + 1.0, CONTENT_Y + 20.0, 8.0, 60.0, DARKGRAY);
        }
    }

    // Kontrollera om man klickar på stäng-knappen eller drag i analysfönstret
    fn handle_analysis_window_click(&mut self, mouse_pos: (f32, f32)) -> bool {
        if self.game_analysis.is_some() {
            let (mouse_x, mouse_y) = mouse_pos;
            const WINDOW_WIDTH: f32 = 600.0;
            const WINDOW_X: f32 = 300.0;
            const WINDOW_Y: f32 = 75.0;
            const WINDOW_HEIGHT: f32 = 700.0;
            
            // Stäng-knapp
            let close_x = WINDOW_X + WINDOW_WIDTH - 40.0;
            let close_y = WINDOW_Y + 10.0;
            
            if mouse_x >= close_x && mouse_x <= close_x + 30.0 &&
               mouse_y >= close_y && mouse_y <= close_y + 30.0 {
                return true; // Stäng fönstret
            }
            
            // Tillbaka-knapp (om vi är i review-läge)
            if self.review_mode {
                let back_x = WINDOW_X + WINDOW_WIDTH - 80.0;
                let back_y = WINDOW_Y + 10.0;
                
                if mouse_x >= back_x && mouse_x <= back_x + 35.0 &&
                   mouse_y >= back_y && mouse_y <= back_y + 30.0 {
                    self.exit_review_mode();
                    return false; // Håll fönstret öppet
                }
            }
            
            // Kontrollera klick på drag i listan
            const CONTENT_X: f32 = WINDOW_X + 20.0;
            const CONTENT_Y: f32 = WINDOW_Y + 50.0;
            const CONTENT_WIDTH: f32 = WINDOW_WIDTH - 60.0;
            const CONTENT_HEIGHT: f32 = WINDOW_HEIGHT - 70.0;
            
            // Kontrollera om klicket är inom innehållsområdet
            if mouse_x >= CONTENT_X && mouse_x <= CONTENT_X + CONTENT_WIDTH &&
               mouse_y >= CONTENT_Y && mouse_y <= CONTENT_Y + CONTENT_HEIGHT {
                
                // Beräkna ungefär var draglistan börjar (efter sammanfattning och färgförklaring)
                let drag_list_start_y = CONTENT_Y + 20.0 + // Sammanfattning titel
                    25.0 + 18.0 * 5.0 + 30.0 + // Sammanfattning innehåll
                    25.0 + 18.0 + 15.0 * 4.0 + 25.0 + 15.0; // Färgförklaring
                
                if mouse_y >= drag_list_start_y {
                    // Beräkna vilket drag som klickades
                    let relative_y = mouse_y - drag_list_start_y;
                    let line_height = 18.0;
                    
                    // Ungefärligt dragindex (tar hänsyn till extra rader för bästa drag)
                    let estimated_move_index = (relative_y / line_height) as usize;
                    
                    // Begränsa till faktiska drag
                    if let Some(ref analysis) = self.game_analysis {
                        if estimated_move_index < analysis.moves.len() {
                            self.show_position_at_move(estimated_move_index);
                        }
                    }
                }
            }
        }
        false
    }

    // Konvertera Square till (x, y) koordinater med hänsyn till rotation
    fn square_to_coords(&self, square: Square) -> (i32, i32) {
        let file = square.get_file().to_index() as i32;
        let rank = square.get_rank().to_index() as i32;
        
        if self.settings.board_flipped {
            (7 - file, rank)
        } else {
            (file, 7 - rank)
        }
    }

    // Konvertera koordinater tillbaka till Square
    fn coords_to_square(&self, x: i32, y: i32) -> Square {
        let (file_idx, rank_idx) = if self.settings.board_flipped {
            (7 - x, y)
        } else {
            (x, 7 - y)
        };
        
        let file = chess::File::from_index(file_idx as usize);
        let rank = chess::Rank::from_index(rank_idx as usize);
        Square::make_square(rank, file)
    }

    // Kontrollera om ett drag är lagligt
    fn is_legal_move(&self, chess_move: ChessMove) -> bool {
        let movegen = MoveGen::new_legal(&self.board);
        movegen.into_iter().any(|m| m == chess_move)
    }

    // Rita koordinater runt brädet
    fn draw_coordinates(&self) {
        const BOARD_OFFSET: f32 = 100.0;
        const SQUARE_SIZE: f32 = 80.0;
        
        // Rita filbeteckningar (a-h)
        for i in 0..8 {
            let file_char = if self.settings.board_flipped {
                char::from(b'h' - i as u8)
            } else {
                char::from(b'a' + i as u8)
            };
            
            let x = BOARD_OFFSET + i as f32 * SQUARE_SIZE + SQUARE_SIZE / 2.0 - 5.0;
            
            // Under brädet
            let y_bottom = BOARD_OFFSET + 8.0 * SQUARE_SIZE + 25.0;
            draw_text(&file_char.to_string(), x, y_bottom, 24.0, BLACK);
            
            // Över brädet
            let y_top = BOARD_OFFSET - 10.0;
            draw_text(&file_char.to_string(), x, y_top, 24.0, BLACK);
        }
        
        // Rita radbeteckningar (1-8)
        for i in 0..8 {
            let rank = if self.settings.board_flipped {
                (i + 1).to_string()
            } else {
                (8 - i).to_string()
            };
            
            let y = BOARD_OFFSET + i as f32 * SQUARE_SIZE + SQUARE_SIZE / 2.0 + 8.0;
            
            // Till vänster om brädet
            let x_left = BOARD_OFFSET - 25.0;
            draw_text(&rank, x_left, y, 24.0, BLACK);
            
            // Till höger om brädet
            let x_right = BOARD_OFFSET + 8.0 * SQUARE_SIZE + 15.0;
            draw_text(&rank, x_right, y, 24.0, BLACK);
        }
    }

    // Rita schackpjäserna
    fn draw_pieces(&self) {
        const PIECE_SIZE: f32 = 75.0;
        const SQUARE_SIZE: f32 = 80.0;
        const BOARD_OFFSET: f32 = 100.0;
        
        // Använd display_board istället för self.board
        let display_board = self.get_display_board();
        
        for square in chess::ALL_SQUARES {
            if let Some(piece) = display_board.piece_on(square) {
                let color = display_board.color_on(square).unwrap();
                let (x, y) = self.square_to_coords(square);
                
                let screen_x = x as f32 * SQUARE_SIZE + BOARD_OFFSET;
                let screen_y = y as f32 * SQUARE_SIZE + BOARD_OFFSET;
                
                let piece_key = PieceKey { piece, color };
                
                // Om vi har en textur för denna pjäs, använd den
                if let Some(texture) = self.textures.get(&piece_key) {
                    let offset = (SQUARE_SIZE - PIECE_SIZE) / 2.0;
                    draw_texture_ex(
                        texture, 
                        screen_x + offset, 
                        screen_y + offset, 
                        WHITE,
                        DrawTextureParams {
                            dest_size: Some(Vec2::new(PIECE_SIZE, PIECE_SIZE)),
                            ..Default::default()
                        }
                    );
                } else {
                    // Fallback till symboler
                    let piece_color = if color == ChessColor::White { WHITE } else { BLACK };
                    
                    draw_circle(screen_x + 40.0, screen_y + 40.0, 25.0, piece_color);
                    draw_circle_lines(screen_x + 40.0, screen_y + 40.0, 25.0, 2.0, DARKGRAY);
                    
                    let symbol = match piece {
                        Piece::Pawn => "♟",
                        Piece::Rook => "♜",
                        Piece::Knight => "♞",
                        Piece::Bishop => "♝",
                        Piece::Queen => "♛",
                        Piece::King => "♚",
                    };
                    
                    let text_color = if color == ChessColor::White { BLACK } else { WHITE };
                    draw_text(symbol, screen_x + 30.0, screen_y + 45.0, 30.0, text_color);
                }
            }
        }
    }

    // Hantera musklick
    fn handle_mouse_click(&mut self, mouse_pos: (f32, f32), ai_controller: &Option<ThreadSafeAiController>) {
        // Kontrollera först om analysfönstret är öppet och om man klickar på stäng-knappen
        if self.handle_analysis_window_click(mouse_pos) {
            self.game_analysis = None; // Stäng analysfönstret
            return;
        }
        
        // Om analysfönstret är öppet, blockera all annan interaktion
        if self.game_analysis.is_some() {
            return;
        }
        
        // Hantera UI-knappar
        if self.resign_button.is_clicked() && !self.game_over {
            self.resign();
            return;
        }
        
        if self.export_button.is_clicked() {
            self.export_pgn();
            return;
        }
        
        if self.flip_button.is_clicked() {
            self.settings.board_flipped = !self.settings.board_flipped;
            return;
        }
        
        if self.white_button.is_clicked() && !matches!(self.ai_state, AiState::Thinking(_)) {
            self.settings.player_color = ChessColor::White;
            return;
        }
        
        if self.black_button.is_clicked() && !matches!(self.ai_state, AiState::Thinking(_)) {
            self.settings.player_color = ChessColor::Black;
            return;
        }
        
        if self.new_game_button.is_clicked() {
            self.reset_game();
            return;
        }
        
        if self.analyze_button.is_clicked() {
            if let Some(ai) = ai_controller {
                if !self.move_history.is_empty() {
                    self.start_full_game_analysis(ai);
                } else {
                    self.start_analysis(ai);
                }
            }
            return;
        }

        // Hantera drag på brädet (endast om vi inte är i review-läge)
        if self.review_mode {
            return; // Blockera dragning när vi tittar på historiska positioner
        }
        
        if self.game_over || self.board.side_to_move() != self.settings.player_color {
            return;
        }

        let (mouse_x, mouse_y) = mouse_pos;
        const BOARD_OFFSET: f32 = 100.0;
        const BOARD_SIZE: f32 = 640.0;
        
        if mouse_x < BOARD_OFFSET || mouse_x > BOARD_OFFSET + BOARD_SIZE || 
           mouse_y < BOARD_OFFSET || mouse_y > BOARD_OFFSET + BOARD_SIZE {
            return;
        }

        let board_x = ((mouse_x - BOARD_OFFSET) / 80.0) as i32;
        let board_y = ((mouse_y - BOARD_OFFSET) / 80.0) as i32;
        
        if board_x < 0 || board_x >= 8 || board_y < 0 || board_y >= 8 {
            return;
        }

        let clicked_square = self.coords_to_square(board_x, board_y);

        if let Some(selected) = self.selected_square {
            let chess_move = ChessMove::new(selected, clicked_square, None);
            
            if self.is_legal_move(chess_move) {
                self.make_move(chess_move);
            } else {
                if self.board.piece_on(clicked_square).is_some() && 
                   self.board.color_on(clicked_square) == Some(self.settings.player_color) {
                    self.selected_square = Some(clicked_square);
                    self.update_highlighted_moves();
                } else {
                    self.selected_square = None;
                    self.highlighted_moves.clear();
                }
            }
        } else {
            if self.board.piece_on(clicked_square).is_some() && 
               self.board.color_on(clicked_square) == Some(self.settings.player_color) {
                self.selected_square = Some(clicked_square);
                self.update_highlighted_moves();
            }
        }
    }

    fn update_highlighted_moves(&mut self) {
        self.highlighted_moves.clear();
        if let Some(selected) = self.selected_square {
            let movegen = MoveGen::new_legal(&self.board);
            
            for m in movegen {
                if m.get_source() == selected {
                    self.highlighted_moves.push(m);
                }
            }
        }
    }

    // Rita markerad ruta och möjliga drag
    fn draw_highlights(&self) {
        const BOARD_OFFSET: f32 = 100.0;
        const SQUARE_SIZE: f32 = 80.0;
        
        if let Some(selected) = self.selected_square {
            let (x, y) = self.square_to_coords(selected);
            draw_rectangle_lines(
                x as f32 * SQUARE_SIZE + BOARD_OFFSET,
                y as f32 * SQUARE_SIZE + BOARD_OFFSET,
                SQUARE_SIZE,
                SQUARE_SIZE,
                4.0,
                YELLOW
            );
        }

        for m in &self.highlighted_moves {
            let (x, y) = self.square_to_coords(m.get_dest());
            draw_circle(
                x as f32 * SQUARE_SIZE + BOARD_OFFSET + SQUARE_SIZE / 2.0,
                y as f32 * SQUARE_SIZE + BOARD_OFFSET + SQUARE_SIZE / 2.0,
                10.0,
                GREEN
            );
        }
    }

    fn update(&mut self) {
        self.depth_slider.update();
        
        // Uppdatera knappstatus
        self.resign_button.set_active(!self.game_over);
        self.white_button.set_active(!matches!(self.ai_state, AiState::Thinking(_)) && self.settings.player_color != ChessColor::White);
        self.black_button.set_active(!matches!(self.ai_state, AiState::Thinking(_)) && self.settings.player_color != ChessColor::Black);
        self.analyze_button.set_active(matches!(self.ai_state, AiState::Idle) && !self.analysis_in_progress);
    }

    fn draw_control_panel(&self) {
        const PANEL_X: f32 = 780.0;
        const PANEL_WIDTH: f32 = 200.0;
        
        // Rita panelbakgrund
        draw_rectangle(PANEL_X - 10.0, 50.0, PANEL_WIDTH, 750.0, Color::new(0.95, 0.95, 0.95, 1.0));
        draw_rectangle_lines(PANEL_X - 10.0, 50.0, PANEL_WIDTH, 750.0, 2.0, DARKGRAY);
        
        // Titel
        draw_text("KONTROLLPANEL", PANEL_X, 80.0, 20.0, BLACK);
        
        // AI-sökdjup slider
        self.depth_slider.draw("AI Sökdjup:");
        
        // Knappar
        self.resign_button.draw();
        self.export_button.draw();
        self.flip_button.draw();
        self.white_button.draw();
        self.black_button.draw();
        self.new_game_button.draw();
        self.analyze_button.draw();
        
        // Spelstatus
        let mut y_pos = 370.0;
        draw_text("STATUS:", PANEL_X, y_pos, 16.0, BLACK);
        y_pos += 25.0;
        
        // Visa olika status beroende på läge
        if self.review_mode {
            draw_text("GRANSKNINGSLÄGE", PANEL_X, y_pos, 14.0, BLUE);
            y_pos += 20.0;
            
            if let Some(move_index) = self.review_move_index {
                draw_text(&format!("Visar drag: {}", move_index + 1), PANEL_X, y_pos, 14.0, DARKGRAY);
                y_pos += 20.0;
                
                if let Some(move_str) = self.move_history.get(move_index) {
                    draw_text(&format!("Drag: {}", move_str), PANEL_X, y_pos, 14.0, DARKGRAY);
                    y_pos += 20.0;
                }
            }
            
            let display_board = self.get_display_board();
            draw_text(&format!("Position: {:?} att dra", display_board.side_to_move()), PANEL_X, y_pos, 14.0, DARKGRAY);
            y_pos += 20.0;
            
        } else {
            draw_text(&format!("Tur: {:?}", self.board.side_to_move()), PANEL_X, y_pos, 14.0, DARKGRAY);
            y_pos += 20.0;
            
            draw_text(&format!("Du spelar: {:?}", self.settings.player_color), PANEL_X, y_pos, 14.0, DARKGRAY);
            y_pos += 20.0;
        }
        
        if !self.ai_status().is_empty() {
            draw_text(&self.ai_status(), PANEL_X, y_pos, 14.0, BLUE);
            y_pos += 20.0;
        }
        
        // Analysresultat för enskild position
        if let Some(ref analysis) = self.current_analysis {
            if analysis.contains("Bästa drag:") {
                draw_text("POSITIONSANALYS:", PANEL_X, y_pos, 16.0, BLACK);
                y_pos += 25.0;
                
                // Rita analysen i en ruta
                let analysis_lines: Vec<&str> = analysis.split('\n').collect();
                let analysis_height = analysis_lines.len() as f32 * 15.0 + 10.0;
                
                draw_rectangle(PANEL_X, y_pos, 160.0, analysis_height, Color::new(0.9, 0.9, 1.0, 1.0));
                draw_rectangle_lines(PANEL_X, y_pos, 160.0, analysis_height, 1.0, BLUE);
                
                let mut line_y = y_pos + 15.0;
                for line in analysis_lines {
                    draw_text(line, PANEL_X + 5.0, line_y, 12.0, DARKBLUE);
                    line_y += 15.0;
                }
                
                y_pos += analysis_height + 20.0;
            } else {
                // Visa andra typer av analysmeddelanden
                draw_text("ANALYS:", PANEL_X, y_pos, 16.0, BLACK);
                y_pos += 25.0;
                draw_text(analysis, PANEL_X, y_pos, 12.0, DARKBLUE);
                y_pos += 30.0;
            }
        }
        
        if self.game_over {
            draw_text("SPEL ÖVER", PANEL_X, y_pos, 16.0, RED);
            y_pos += 25.0;
            
            match self.board.status() {
                BoardStatus::Checkmate => {
                    let winner = if self.board.side_to_move() == ChessColor::White { "Svart" } else { "Vit" };
                    draw_text(&format!("{} vann!", winner), PANEL_X, y_pos, 14.0, RED);
                }
                BoardStatus::Stalemate => {
                    draw_text("Patt - Oavgjort", PANEL_X, y_pos, 14.0, ORANGE);
                }
                _ => {
                    if !self.move_history.is_empty() {
                        if let Some(last_move) = self.move_history.last() {
                            if last_move.contains("uppgivning") {
                                draw_text("Uppgivning", PANEL_X, y_pos, 14.0, RED);
                            }
                        }
                    }
                }
            }
            y_pos += 30.0;
        }
        
        // Draglista med färgkodning för analyserade drag
        y_pos += 10.0;
        draw_text("DRAGLISTA:", PANEL_X, y_pos, 16.0, BLACK);
        y_pos += 25.0;
        
        // Rita ruta för draglistan
        let list_height = 200.0; // Mindre höjd för att få plats med analysen
        draw_rectangle(PANEL_X, y_pos, 160.0, list_height, WHITE);
        draw_rectangle_lines(PANEL_X, y_pos, 160.0, list_height, 1.0, DARKGRAY);
        
        // Visa de senaste dragen med färgkodning
        let visible_moves = 12;
        let start_index = if self.move_history.len() > visible_moves {
            self.move_history.len() - visible_moves
        } else {
            0
        };
        
        let mut list_y = y_pos + 20.0;
        for (i, move_str) in self.move_history.iter().enumerate().skip(start_index) {
            if list_y > y_pos + list_height - 20.0 {
                break;
            }
            
            let move_number = i + 1;
            let display_text = if move_str.contains("uppgivning") {
                move_str.clone()
            } else {
                format!("{}. {}", move_number, move_str)
            };
            
            // Bestäm färg baserat på analys
            let text_color = if let Some(ref analysis) = self.game_analysis {
                if let Some(move_analysis) = analysis.moves.get(i) {
                    if move_analysis.is_blunder {
                        RED
                    } else if move_analysis.is_mistake {
                        ORANGE
                    } else if move_analysis.is_inaccuracy {
                        Color::new(0.8, 0.8, 0.0, 1.0) // Gul
                    } else {
                        BLACK
                    }
                } else {
                    BLACK
                }
            } else {
                BLACK
            };
            
            draw_text(&display_text, PANEL_X + 5.0, list_y, 12.0, text_color);
            list_y += 15.0;
        }
        
        // Visa totalt antal drag
        draw_text(
            &format!("Totalt: {} drag", self.move_history.len()),
            PANEL_X,
            y_pos + list_height + 20.0,
            12.0,
            DARKGRAY
        );
    }
}

// =============================================================
// DEL 5: FÖNSTERKONFIGURATION & HUVUDFUNKTION
// =============================================================

fn window_conf() -> Conf {
    Conf {
        window_title:  "Daniels schack - powered by Stockfish 17.1 i Rust - 2025 - v1.0".into(),
        window_width:  1000, // Återställ till ursprunglig bredd
        window_height: 850,
        ..Default::default()
    }
}

async fn load_piece_textures() -> HashMap<PieceKey, Texture2D> {
    let mut textures = HashMap::new();
    
    // Lista över alla pjäser och deras filnamn
    let pieces = [
        (Piece::King, ChessColor::White, "assets/white_king.png"),
        (Piece::Queen, ChessColor::White, "assets/white_queen.png"),
        (Piece::Rook, ChessColor::White, "assets/white_rook.png"),
        (Piece::Bishop, ChessColor::White, "assets/white_bishop.png"),
        (Piece::Knight, ChessColor::White, "assets/white_knight.png"),
        (Piece::Pawn, ChessColor::White, "assets/white_pawn.png"),
        (Piece::King, ChessColor::Black, "assets/black_king.png"),
        (Piece::Queen, ChessColor::Black, "assets/black_queen.png"),
        (Piece::Rook, ChessColor::Black, "assets/black_rook.png"),
        (Piece::Bishop, ChessColor::Black, "assets/black_bishop.png"),
        (Piece::Knight, ChessColor::Black, "assets/black_knight.png"),
        (Piece::Pawn, ChessColor::Black, "assets/black_pawn.png"),
    ];

    for (piece, color, filename) in pieces.iter() {
        match load_texture(filename).await {
            Ok(texture) => {
                texture.set_filter(FilterMode::Linear);
                textures.insert(PieceKey { piece: *piece, color: *color }, texture);
                println!("✓ Laddade textur: {}", filename);
            }
            Err(e) => {
                eprintln!("⚠ Kunde inte ladda {}: {}", filename, e);
            }
        }
    }
    
    println!("Totalt {} texturer laddade", textures.len());
    textures
}

#[macroquad::main(window_conf)]
async fn main() {
    // ===== En‑instans‑lås ====================================
    let instance = SingleInstance::new("chess_macroquad_instance").expect("kunde inte skapa låsfil");
    if !instance.is_single() {
        eprintln!("Programmet kör redan – avslutar.");
        return;
    }

    println!("\n========================================\n  Programstart – initierar spel\n========================================\n");
    println!("PID: {}", std::process::id());

    // Försök starta Stockfish med timeout
    println!("Försöker starta Stockfish...");
    let ai_controller = match ThreadSafeAiController::new() {
        Ok(ctrl) => {
            println!("✓ Stockfish startad framgångsrikt!");
            Some(ctrl)
        },
        Err(e) => {
            eprintln!("⚠ Kunde inte starta Stockfish: {e}");
            eprintln!("⚠ Spelet fortsätter utan AI (endast manuellt spel)");
            None
        }
    };

    let mut game = ChessGame::new(load_piece_textures().await);
    println!("✓ Schackspel initierat!");

    // =========================================================
    // HUVUDLOOP – körs varje bildruta
    // =========================================================
    loop {
        clear_background(Color::new(0.9, 0.9, 0.9, 1.0));

        // 1) Uppdatera UI-komponenter
        game.update();

        // 2) Hantera musklick
        if is_mouse_button_pressed(MouseButton::Left) {
            game.handle_mouse_click(mouse_position(), &ai_controller);
        }

        // 3) Poll AI för drag
        game.poll_ai();

        // 4) Poll partianalys
        game.poll_analysis();

        // 5) Start AI om det är dess tur
        if game.is_ai_turn() {
            if let Some(ref ai) = ai_controller {
                game.start_ai(ai);
            }
        }

        // 6) Rita brädet 8×8
        const BOARD_OFFSET: f32 = 100.0;
        const SQUARE_SIZE: f32 = 80.0;
        for y in 0..8 {
            for x in 0..8 {
                let c = if (x + y) % 2 == 0 { BEIGE } else { BROWN };
                draw_rectangle(
                    x as f32 * SQUARE_SIZE + BOARD_OFFSET, 
                    y as f32 * SQUARE_SIZE + BOARD_OFFSET, 
                    SQUARE_SIZE, 
                    SQUARE_SIZE, 
                    c
                );
            }
        }

        // 7) Rita koordinater
        game.draw_coordinates();

        // 8) Rita markeringar
        game.draw_highlights();

        // 9) Rita pjäserna
        game.draw_pieces();

        // 10) Rita kontrollpanel
        game.draw_control_panel();

        // 11) Rita analysfönster som overlay (om det finns)
        game.draw_analysis_window();

        // 12) Rita huvudtitel
        draw_text("SCHACKSPEL", 10.0, 30.0, 24.0, BLACK);

        // Debug-information längst ner
        let debug_text = format!(
            "Stockfish: {} | Bräde roterat: {} | Spelare: {:?} | Analys: {}",
            if ai_controller.is_some() { "Aktiv" } else { "Ej tillgänglig" },
            game.settings.board_flipped,
            game.settings.player_color,
            if game.analysis_in_progress { "Pågår" } else if game.game_analysis.is_some() { "Klar" } else { "Ingen" }
        );
        draw_text(&debug_text, 10.0, 820.0, 12.0, DARKGRAY);

        next_frame().await;
    }
}
#![no_std]
#![no_main]

use core::ptr::addr_of_mut;
use critical_section::CriticalSection;
use esp_hal::{delay::Delay, gpio::Io, i2c::{self, master::{Config, I2c}}, peripheral, peripherals::{Peripherals, GPIO}};
use bleps::{
    ad_structure::{
        create_advertising_data,
        AdStructure,
        BR_EDR_NOT_SUPPORTED,
        LE_GENERAL_DISCOVERABLE,
    },
    attribute_server::{AttributeServer, NotificationData, WorkResult},
    gatt,
    Ble,
    HciConnector,
};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    rng::Rng,
    system::{Cpu, CpuControl, Stack},
    time,
    timer::{timg::TimerGroup, AnyTimer},
};
use esp_println::println;
use esp_wifi::{ble::controller::BleConnector, init, EspWifiController};

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_hal_embassy::Executor;
use heapless::String;
use static_cell::StaticCell;

use esp_hal::peripherals::BT;

use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};

use hd44780_driver::{HD44780, memory_map::MemoryMap1602, setup::DisplayOptionsI2C, DisplayMode, Display, Cursor, CursorBlink};

use embassy_sync::channel::Channel;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;

use core::cell::RefCell;

#[derive(Debug, Clone)]
pub enum BleCommand {
    SendData(heapless::Vec<u8, 256>), // Data to send via BLE
    UpdateCharacteristic(u8, heapless::Vec<u8, 256>), // Characteristic ID and data
}

#[derive(Debug, Clone)]
pub enum BleEvent {
    DataReceived(heapless::Vec<u8, 256>),
    ClientConnected,
    ClientDisconnected,
    CharacteristicWritten(u8, heapless::Vec<u8, 256>), // Characteristic ID and data
}

// Create global channels (in your main.rs or lib.rs)
static BLE_COMMAND_CHANNEL: Channel<CriticalSectionRawMutex, BleCommand, 10> = Channel::new();
static BLE_EVENT_CHANNEL: Channel<CriticalSectionRawMutex, BleEvent, 10> = Channel::new();

// Chess piece types
#[derive(Debug, Clone, Copy, PartialEq)]
enum PieceType {
    Pawn = 0,
    Knight = 1,
    Bishop = 2,
    Rook = 3,
    Queen = 4,
    King = 5,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Color {
    White = 0,
    Black = 1,
}

impl Color {
    fn opposite(&self) -> Color {
        match self {
            Color::White => Color::Black,
            Color::Black => Color::White,
        }
    }

    fn to_str(&self) -> &'static str {
        match self {
            Color::White => "White",
            Color::Black => "Black",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Piece {
    piece_type: PieceType,
    color: Color,
}

impl Piece {
    pub fn to_str(&self) -> &'static str {
        match (self.color, self.piece_type) {
            (Color::White, PieceType::Pawn) => "White Pawn",
            (Color::Black, PieceType::Pawn) => "Black Pawn",
            (Color::White, PieceType::Knight) => "White Knight",
            (Color::Black, PieceType::Knight) => "Black Knight",
            (Color::White, PieceType::Bishop) => "White Bishop",
            (Color::Black, PieceType::Bishop) => "Black Bishop",
            (Color::White, PieceType::Rook) => "White Rook",
            (Color::Black, PieceType::Rook) => "Black Rook",
            (Color::White, PieceType::Queen) => "White Queen",
            (Color::Black, PieceType::Queen) => "Black Queen",
            (Color::White, PieceType::King) => "White King",
            (Color::Black, PieceType::King) => "Black King",
        }
    }
    pub fn short_to_str(&self) -> &'static str {
        match (self.color, self.piece_type) {
            (Color::White, PieceType::Pawn) => "P",
            (Color::Black, PieceType::Pawn) => "p",
            (Color::White, PieceType::Knight) => "N",
            (Color::Black, PieceType::Knight) => "n",
            (Color::White, PieceType::Bishop) => "B",
            (Color::Black, PieceType::Bishop) => "b",
            (Color::White, PieceType::Rook) => "R",
            (Color::Black, PieceType::Rook) => "r",
            (Color::White, PieceType::Queen) => "Q",
            (Color::Black, PieceType::Queen) => "q",
            (Color::White, PieceType::King) => "K",
            (Color::Black, PieceType::King) => "k",
        }
    }
}

// Game state for turn management
#[derive(Debug, Clone, Copy, PartialEq)]
enum GameState {
    WaitingForMove,
    PieceLifted,
    WaitingForCapture,
    WaitingForPlacement,
    WaitingForPromotion,
    OpponentTurn,
}

// In order to check what move was made, we need a "watchdog"
// The structure below stores where the piece was lifted from, what kind of piece it is and whether
// it is waiting to be placed or not
// It can also check for captured pieces and implements a timeout in order to make sure that the game can't
// be stalemated due to a player not capturing a piece
#[derive(Debug)]
struct MoveTracker {
    current_turn: Color,
    game_state: GameState,
    lifted_square: Option<u8>,
    lifted_piece: Option<Piece>,
    captured_square: Option<u8>,
    captured_piece: Option<Piece>,
    promotion_square: Option<u8>,
    capture_timeout: u32, // Timeout counter for capture detection
}

impl MoveTracker {
    const fn new() -> Self {
        MoveTracker {
            current_turn: Color::White, // White starts
            game_state: GameState::WaitingForMove,
            lifted_square: None,
            lifted_piece: None,
            captured_square: None,
            captured_piece: None,
            promotion_square: None,
            capture_timeout: 0,
        }
    }

    // Check if a pawn move results in promotion
    fn is_promotion_move(&self, to_square: u8, piece: Piece) -> bool {
        if piece.piece_type != PieceType::Pawn {
            return false;
        }
        
        match piece.color {
            Color::White => to_square >= 56, // Row 8 (squares 56-63)
            Color::Black => to_square <= 7,  // Row 1 (squares 0-7)
        }
    }

    // The piece_lifted() function is called when a piece is lifted from a square and sets up the tracker as follows:
    // remembers where the piece was, remembers what type of piece it was and marks the fact that the system is waiting
    // for the piece to be placed somewhere
    fn piece_lifted(&mut self, square: u8, piece: Piece) -> MoveResult {
        match self.game_state {
            GameState::WaitingForMove => {
                if piece.color == self.current_turn {
                    self.lifted_square = Some(square);
                    self.lifted_piece = Some(piece);
                    self.game_state = GameState::PieceLifted;
                    self.capture_timeout = 0;
                    MoveResult::PieceLifted(square, piece)
                } else {
                    MoveResult::WrongTurn
                }
            },
            GameState::PieceLifted => {
                // We check whether this is the opponent's piece being captured
                if piece.color != self.current_turn {
                    self.captured_square = Some(square);
                    self.captured_piece = Some(piece);
                    self.game_state = GameState::WaitingForPlacement;
                    self.capture_timeout = 0;
                    MoveResult::PieceCaptured(square, piece)
                } else {
                    // The player changed their mind and lifted a different piece of their own
                    self.lifted_square = Some(square);
                    self.lifted_piece = Some(piece);
                    MoveResult::PieceLifted(square, piece)
                }
            },
            _ => MoveResult::InvalidAction
        }
    }

    // The piece_placed() function is called when a piece is on square and it returns the full move: FROM, TO, TYPE
    // The using the gamestate system, the function detects whether the player is performing a normal move or capturing a piece
    fn piece_placed(&mut self, square: u8) -> MoveResult {
        match self.game_state {
            GameState::PieceLifted => {
                // Normal move (no capture)
                if let (Some(from_square), Some(piece)) = (self.lifted_square, self.lifted_piece) {
                    if self.is_promotion_move(square, piece) {
                        // Pawn promotion
                        self.promotion_square = Some(square);
                        self.game_state = GameState::WaitingForPromotion;
                        MoveResult::PawnPromoted(from_square, square)
                    } else {
                        // Normal move
                        self.complete_move();
                        MoveResult::MoveCompleted(from_square, square, piece, None)
                    }
                } else {
                    MoveResult::InvalidAction
                }
            },
            GameState::WaitingForPlacement => {
                // Move with capture
                if let (Some(from_square), Some(piece), Some(captured_sq), Some(captured_p)) = 
                   (self.lifted_square, self.lifted_piece, self.captured_square, self.captured_piece) {
                    
                    if square == captured_sq {
                        if self.is_promotion_move(square, piece) {
                            // Capture with promotion
                            self.promotion_square = Some(square);
                            self.game_state = GameState::WaitingForPromotion;
                            MoveResult::PawnPromotedWithCapture(from_square, square, captured_p)
                        } else {
                            // Normal capture
                            self.complete_move();
                            MoveResult::MoveCompleted(from_square, square, piece, Some(captured_p))
                        }
                    } else {
                        MoveResult::InvalidAction
                    }
                } else {
                    MoveResult::InvalidAction
                }
            },
            GameState::WaitingForPromotion => {
                // Queen placed on promotion square
                if let (Some(from_square), Some(promotion_sq)) = (self.lifted_square, self.promotion_square) {
                    if square == promotion_sq {
                        let queen = Piece { piece_type: PieceType::Queen, color: self.current_turn };
                        let captured = self.captured_piece;
                        self.complete_move();
                        MoveResult::PromotionCompleted(from_square, square, queen, captured)
                    } else {
                        MoveResult::InvalidAction
                    }
                } else {
                    MoveResult::InvalidAction
                }
            },
            _ => MoveResult::InvalidAction
        }
    }

    // Complete the move and switch turns
    fn complete_move(&mut self) {
        self.current_turn = self.current_turn.opposite();
        self.game_state = GameState::WaitingForMove;
        self.lifted_square = None;
        self.lifted_piece = None;
        self.captured_square = None;
        self.captured_piece = None;
        self.promotion_square = None;
        self.capture_timeout = 0;
    }

    // Handle timeout for capture detection
    fn update_timeout(&mut self) -> Option<MoveResult> {
        if self.game_state == GameState::PieceLifted {
            self.capture_timeout += 1;
            // If no capture detected after reasonable time, treat as normal move
            if self.capture_timeout > 100 { // ~5 seconds at 50ms intervals
                self.game_state = GameState::WaitingForPlacement;
                return Some(MoveResult::TimeoutReached);
            }
        }
        None
    }

    // Cancel current move
    fn cancel_move(&mut self) -> MoveResult {
        self.game_state = GameState::WaitingForMove;
        self.lifted_square = None;
        self.lifted_piece = None;
        self.captured_square = None;
        self.captured_piece = None;
        self.promotion_square = None;
        self.capture_timeout = 0;
        MoveResult::MoveCancelled
    }

    fn get_current_turn(&self) -> Color {
        self.current_turn
    }

    fn get_game_state(&self) -> GameState {
        self.game_state
    }
}

// Result types for move operations
#[derive(Debug)]
enum MoveResult {
    PieceLifted(u8, Piece),
    PieceCaptured(u8, Piece),
    MoveCompleted(u8, u8, Piece, Option<Piece>),      // from, to, piece, captured_piece
    PawnPromoted(u8, u8),                             // from, to
    PawnPromotedWithCapture(u8, u8, Piece),           // from, to, captured_piece
    PromotionCompleted(u8, u8, Piece, Option<Piece>), // from, to, promoted_piece, captured_piece
    MoveCancelled,
    TimeoutReached,
    WrongTurn,
    InvalidAction,
}

// Chess board state using bitboards
#[derive(Debug)]
struct ChessBoard {
    // Bitboards for each piece type and color
    white_pawns: u64,
    white_knights: u64,
    white_bishops: u64,
    white_rooks: u64,
    white_queens: u64,
    white_king: u64,
    black_pawns: u64,
    black_knights: u64,
    black_bishops: u64,
    black_rooks: u64,
    black_queens: u64,
    black_king: u64,
}

impl ChessBoard {
    // Initialize with standard chess starting position
    const fn new() -> Self {
        ChessBoard {
            // Bitboard constants for the initial position
            // In a nutshell, each of these hexadecimal number can be expressed in binary
            // Since a chess board has 64 values, a u64 data type also has 64 bits, which can be modified
            // According to the position of each piece on the board

            white_pawns: 0x000000000000FF00,    // Row 2
            white_knights: 0x0000000000000042,  // B1, G1
            white_bishops: 0x0000000000000024,  // C1, F1
            white_rooks: 0x0000000000000081,    // A1, H1
            white_queens: 0x0000000000000010,  // D1
            white_king: 0x0000000000000008,     // E1
            black_pawns: 0x00FF000000000000,    // Row 7
            black_knights: 0x4200000000000000,  // B8, G8
            black_bishops: 0x2400000000000000,  // C8, F8
            black_rooks: 0x8100000000000000,    // A8, H8
            black_queens:0x1000000000000000,   // D8
            black_king: 0x0800000000000000,     // E8

            // Let's take white_pawns as an example. Its value is 0x0000_0000_0000_FF00
            // Translating it into binary would be:
            // -> 000000000000FF00 
            // -> 0 0 0 0 0 0 0 0 0 0 0 0 F F 0 0
            // -> 0000 0000 0000 0000 0000 0000 0000 0000 0000 0000 0000 0000 1111 1111 0000 0000

            // By rearangin, we get the chess board in bit format:
            // 0 0 0 0 0 0 0 0
            // 0 0 0 0 0 0 0 0
            // 0 0 0 0 0 0 0 0
            // 0 0 0 0 0 0 0 0
            // 0 0 0 0 0 0 0 0
            // 0 0 0 0 0 0 0 0
            // 1 1 1 1 1 1 1 1
            // 0 0 0 0 0 0 0 0
        }
    }

    
    fn get_piece_at(&self, square: u8) -> Option<Piece> {
        // The mask variable is nothing more than a simple u64 whose least significant bit (bit 0) is 1
    // "square" is a number that takes values from 0 to 63
    // By using the left shift bitwise operation we basically shift the least significand bit by "square" positions to the left
        let mask = 1u64 << square;
        

        // We know that each piece has a bitboard associated to it.
        // The "mask" itself is a bitboard as well.
        // We know that in a hexadecimal number, each digit represents 4 bits when converted to decimal
        // F -> 1111
        // E -> 1110
        // ...
        // 0 -> 0000
        
        // 63 62 61 60 59 58 57 56  -> rank 8
        // 55 54 53 52 51 50 49 48  -> rank 7
        // 47 46 45 44 43 42 41 40  -> rank 6
        // 39 38 37 36 35 34 33 32  -> rank 5
        // 31 30 29 28 27 26 25 24  -> rank 4
        // 23 22 21 20 19 18 17 16  -> rank 3
        // 15 14 13 12 11 10 9  8   -> rank 2
        // 7  6  5  4  3  2  1  0   -> rank 1

        // For example:
        // We assume the classic starting position of any chess game
        // We want to check whether there is any piece on the square D2
        // Assuming LSB = a1 (least significant bit)
        // E2 -> 12
        // Shifting 12 bits to the left would mean:
        // mask = 0x0000_0000_0000_1000 = 0000 0000 0000 0000 0000 0000 0000 0000 0000 0000 0000 0000 0001 0000 0000 0000
        // white_pawns: 0x0000_0000_0000_FF00 = 0000 0000 0000 0000 0000 0000 0000 0000 0000 0000 0000 0000 1111 1111 0000 0000
        // By using the bitwise AND operation, we can see if "mask" coincides with any of our bitboards:
        // mask & white_pawns = 0000 0000 0000 0000 0000 0000 0000 0000 0000 0000 0000 0000 0000 0001 0000 0000 != 0
        // So that means that there is a white pawn at that specific location

        // White pieces
        if self.white_pawns & mask != 0 { return Some(Piece { piece_type: PieceType::Pawn, color: Color::White }); }
        if self.white_knights & mask != 0 { return Some(Piece { piece_type: PieceType::Knight, color: Color::White }); }
        if self.white_bishops & mask != 0 { return Some(Piece { piece_type: PieceType::Bishop, color: Color::White }); }
        if self.white_rooks & mask != 0 { return Some(Piece { piece_type: PieceType::Rook, color: Color::White }); }
        if self.white_queens & mask != 0 { return Some(Piece { piece_type: PieceType::Queen, color: Color::White }); }
        if self.white_king & mask != 0 { return Some(Piece { piece_type: PieceType::King, color: Color::White }); }
        

        // Black pieces
        if self.black_pawns & mask != 0 { return Some(Piece { piece_type: PieceType::Pawn, color: Color::Black }); }
        if self.black_knights & mask != 0 { return Some(Piece { piece_type: PieceType::Knight, color: Color::Black }); }
        if self.black_bishops & mask != 0 { return Some(Piece { piece_type: PieceType::Bishop, color: Color::Black }); }
        if self.black_rooks & mask != 0 { return Some(Piece { piece_type: PieceType::Rook, color: Color::Black }); }
        if self.black_queens & mask != 0 { return Some(Piece { piece_type: PieceType::Queen, color: Color::Black }); }
        if self.black_king & mask != 0 { return Some(Piece { piece_type: PieceType::King, color: Color::Black }); }
        
        None
    }

    // Remove piece from a square
    fn remove_piece(&mut self, square: u8) {
        let mask = !(1u64 << square);
        self.white_pawns &= mask;
        self.white_knights &= mask;
        self.white_bishops &= mask;
        self.white_rooks &= mask;
        self.white_queens &= mask;
        self.white_king &= mask;
        self.black_pawns &= mask;
        self.black_knights &= mask;
        self.black_bishops &= mask;
        self.black_rooks &= mask;
        self.black_queens &= mask;
        self.black_king &= mask;
    }

    
    fn place_piece(&mut self, square: u8, piece: Piece) {
        // We check the type of peace and color we are going to place and match it from the list below
        let mask = 1u64 << square;
        match (piece.color, piece.piece_type) {
            // We use a bitwise OR on the bitmap in order set the bit that the square points to to a 1
            // Thus marking the fact that a piece has been placed there
            (Color::White, PieceType::Pawn) => self.white_pawns |= mask,
            (Color::White, PieceType::Knight) => self.white_knights |= mask,
            (Color::White, PieceType::Bishop) => self.white_bishops |= mask,
            (Color::White, PieceType::Rook) => self.white_rooks |= mask,
            (Color::White, PieceType::Queen) => self.white_queens |= mask,
            (Color::White, PieceType::King) => self.white_king |= mask,
            (Color::Black, PieceType::Pawn) => self.black_pawns |= mask,
            (Color::Black, PieceType::Knight) => self.black_knights |= mask,
            (Color::Black, PieceType::Bishop) => self.black_bishops |= mask,
            (Color::Black, PieceType::Rook) => self.black_rooks |= mask,
            (Color::Black, PieceType::Queen) => self.black_queens |= mask,
            (Color::Black, PieceType::King) => self.black_king |= mask,
        }
    }

    // Execute a move on the board
    fn execute_move(&mut self, from: u8, to: u8, piece: Piece, captured_piece: Option<Piece>) {
        self.remove_piece(from);
        if captured_piece.is_some() {
            self.remove_piece(to); // Remove captured piece
        }
        self.place_piece(to, piece);
    }

    // Execute a promotion move on the board
    fn execute_promotion(&mut self, from: u8, to: u8, promoted_piece: Piece, captured_piece: Option<Piece>) {
        self.remove_piece(from); // Remove the pawn
        if captured_piece.is_some() {
            self.remove_piece(to); // Remove captured piece if any
        }
        self.place_piece(to, promoted_piece); // Place the queen
    }
}

// Chess square mapping using lookup table for no_std
const SQUARE_NAMES: [&str; 64] = [
    "H1", "G1", "F1", "E1", "D1", "C1", "B1", "A1",
    "H2", "G2", "F2", "E2", "D2", "C2", "B2", "A2",
    "H3", "G3", "F3", "E3", "D3", "C3", "B3", "A3",
    "H4", "G4", "F4", "E4", "D4", "C4", "B4", "A4",
    "H5", "G5", "F5", "E5", "D5", "C5", "B5", "A5",
    "H6", "G6", "F6", "E6", "D6", "C6", "B6", "A6",
    "H7", "G7", "F7", "E7", "D7", "C7", "B7", "A7",
    "H8", "G8", "F8", "E8", "D8", "C8", "B8", "A8",
];

// Get square index from signal pin and channel
fn get_square_index(signal_pin: u8, channel: usize) -> Option<u8> {
    let square_idx = match signal_pin {
        1 => { 
            // sig_pin_1: rows 8 and 7
            if channel < 8 { 56 + channel }             // Row 8
            else if channel < 16 { 48 + (channel - 8) } // Row 7
            else { return None; }
        },
        2 => { 
            // sig_pin_2: rows 6 and 5
            if channel < 8 { 40 + channel }             // Row 6
            else if channel < 16 { 32 + (channel - 8) } // Row 5
            else { return None; }
        },
        3 => { 
            // sig_pin_3: rows 4 and 3
            if channel < 8 { 24 + channel }             // Row 4
            else if channel < 16 { 16 + (channel - 8) } // Row 3
            else { return None; }
        },
        4 => { 
            // sig_pin_4: rows 2 and 1
            if channel < 8 { 8 + channel }              // Row 2
            else if channel < 16 { channel - 8 }        // Row 1
            else { return None; }
        },
        _ => return None,
    };
    
    if square_idx < 64 {
        Some(square_idx as u8)
    } else {
        None
    }
}

// In order to ensure that the system won't stumble if a sensor reads a bad input 
// (short interference with other magnetic fields,electromagnetic noise, etc)
// I created a system that checks 5 outputs of the sensors and make sure that they are all coherent.

// This structure tracks the state of one square
#[derive(Debug, Clone, Copy)]
struct SquareState {
    confirmed_state: bool,
    consecutive_count: u8,
    last_reading: bool,
}

impl SquareState {
    const fn new(initial_state: bool) -> Self {
        SquareState {
            confirmed_state: initial_state,
            consecutive_count: 5, // Start as confirmed
            last_reading: initial_state,
        }
    }

    // The update function is called each "cycle" in order to check whether a sensor registered a new state.
    // If the state is the same, then increase consecutive_count by 1
    // If it is not the same, then it resets consecutive_count back to 1 and stores the new state

    // If we have 5 identical states in a row AND it is different from the confirmed state
    // Then it accepts the new reading as confirmed
    fn update(&mut self, new_reading: bool) -> bool {
        if new_reading == self.last_reading {
            if self.consecutive_count < 255 {
                self.consecutive_count += 1;
            }
        } else {
            self.consecutive_count = 1;
            self.last_reading = new_reading;
        }

        // Require 5 consecutive readings to confirm a state change
        if self.consecutive_count >= 5 && new_reading != self.confirmed_state {
            self.confirmed_state = new_reading;
            return true; // State changed
        }
        
        false
    }
}

// Initialize square states array using const fn for no_std
const fn init_square_states() -> [SquareState; 64] {
    // In starting position, squares with pieces return false (piece present)
    // Empty squares return true
    [
        // Row 1 (0-7): pieces present - false
        SquareState::new(false), SquareState::new(false), SquareState::new(false), SquareState::new(false),
        SquareState::new(false), SquareState::new(false), SquareState::new(false), SquareState::new(false),
        // Row 2 (8-15): pieces present - false  
        SquareState::new(false), SquareState::new(false), SquareState::new(false), SquareState::new(false),
        SquareState::new(false), SquareState::new(false), SquareState::new(false), SquareState::new(false),
        // Rows 3-6 (16-47): empty squares - true
        SquareState::new(true), SquareState::new(true), SquareState::new(true), SquareState::new(true),
        SquareState::new(true), SquareState::new(true), SquareState::new(true), SquareState::new(true),
        SquareState::new(true), SquareState::new(true), SquareState::new(true), SquareState::new(true),
        SquareState::new(true), SquareState::new(true), SquareState::new(true), SquareState::new(true),
        SquareState::new(true), SquareState::new(true), SquareState::new(true), SquareState::new(true),
        SquareState::new(true), SquareState::new(true), SquareState::new(true), SquareState::new(true),
        SquareState::new(true), SquareState::new(true), SquareState::new(true), SquareState::new(true),
        SquareState::new(true), SquareState::new(true), SquareState::new(true), SquareState::new(true),
        // Row 7 (48-55): pieces present - false
        SquareState::new(false), SquareState::new(false), SquareState::new(false), SquareState::new(false),
        SquareState::new(false), SquareState::new(false), SquareState::new(false), SquareState::new(false),
        // Row 8 (56-63): pieces present - false
        SquareState::new(false), SquareState::new(false), SquareState::new(false), SquareState::new(false),
        SquareState::new(false), SquareState::new(false), SquareState::new(false), SquareState::new(false),
    ]
}


#[esp_hal_embassy::main]
async fn main(_spawner: Spawner) {
    // Configure system with default settings and maximum CPU clock
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);
    esp_alloc::heap_allocator!(size: 72 * 1024);



    // Initialize embassy timers
    let mut delay = Delay::new();
    let timg0 = TimerGroup::new(peripherals.TIMG0);




    // Bluetooth configuration and initialization
    let esp_wifi_ctrl = init(
        timg0.timer0,
        Rng::new(peripherals.RNG),
        peripherals.RADIO_CLK,
    ).unwrap();

    esp_hal_embassy::init(timg0.timer1);

    let bluetooth = peripherals.BT;

    // I2C Interface configuration
    let i2c = match I2c::new(
        peripherals.I2C0,
        Config::default(),
    ) {
        Ok(i2c) => i2c.with_sda(peripherals.GPIO11).with_scl(peripherals.GPIO12),
        Err(err) => {
            panic!("Failed to initialize I2C: {:?}", err);
        }
    };


    // Display configuration and initialization
    let mut options = DisplayOptionsI2C::new(MemoryMap1602::new()).with_i2c_bus(i2c, 0x27);
    let mut display = loop {
        match HD44780::new(options, &mut delay) {
            Err((options_back, error)) => {
                options = options_back;
                delay.delay_millis(500);
                println!("{}", error);
            }
            Ok(display) => break display,
        }
    };

    display
        .set_display_mode(
            DisplayMode { display: Display::On, cursor_visibility: Cursor::Invisible, cursor_blink: CursorBlink::Off },
            &mut delay,
        )
        .unwrap();

    display.clear(&mut delay).unwrap();
    display.reset(&mut delay).unwrap();
    display.write_str("Ready to start!", &mut delay).unwrap();


    // Spawn BLE task
    //_spawner.spawn(ble_task(esp_wifi_ctrl, bluetooth)).ok();

    // Each CD74HC4067 Multiplexer has 4 pins dedicated to selecting the channel that the user wants
    // the input/output from
    // Multiplexer control pins
    let mut s0 = Output::new(peripherals.GPIO5, Level::Low, OutputConfig::default());
    let mut s1 = Output::new(peripherals.GPIO6, Level::Low, OutputConfig::default());
    let mut s2 = Output::new(peripherals.GPIO7, Level::Low, OutputConfig::default());
    let mut s3 = Output::new(peripherals.GPIO8, Level::Low, OutputConfig::default());

    // Since the CD74HC4067 Multiplexer has 16 channels, the chessboard is divised into 4 sections.
    // Signal pins from multiplexers
    let sig_pin_1 = Input::new(peripherals.GPIO9, InputConfig::default().with_pull(Pull::Up));
    let sig_pin_2 = Input::new(peripherals.GPIO10, InputConfig::default().with_pull(Pull::Up));
    let sig_pin_3 = Input::new(peripherals.GPIO17, InputConfig::default().with_pull(Pull::Up));
    let sig_pin_4 = Input::new(peripherals.GPIO18, InputConfig::default().with_pull(Pull::Up));

    // The CD74HC4067 is a 16-channel analog multiplexer. It allows me to access independently each
    // channel by using 4 digital control signals (S0-S3).
    // The matrix below stores the binary values for each channel.
    let MUX_CHANNELS: [[u8; 4]; 16] = [
        [0, 0, 0, 0], [1, 0, 0, 0], [0, 1, 0, 0], [1, 1, 0, 0],
        [0, 0, 1, 0], [1, 0, 1, 0], [0, 1, 1, 0], [1, 1, 1, 0],
        [0, 0, 0, 1], [1, 0, 0, 1], [0, 1, 0, 1], [1, 1, 0, 1],
        [0, 0, 1, 1], [1, 0, 1, 1], [0, 1, 1, 1], [1, 1, 1, 1],
    ];

    // Main system initialization
    let mut chess_board = ChessBoard::new();
    let mut square_states = init_square_states();
    let mut move_tracker = MoveTracker::new();

    loop{
        let data_to_send: heapless::Vec<u8, 256> = "Hello from main!".as_bytes().iter().cloned().collect();
        send_ble_command(BleCommand::SendData(data_to_send));
    for channel in 0..16 {
        
        // Set control pins for this channel
        s0.set_level(if MUX_CHANNELS[channel][0] == 1 { Level::High } else { Level::Low });
        s1.set_level(if MUX_CHANNELS[channel][1] == 1 { Level::High } else { Level::Low });
        s2.set_level(if MUX_CHANNELS[channel][2] == 1 { Level::High } else { Level::Low });
        s3.set_level(if MUX_CHANNELS[channel][3] == 1 { Level::High } else { Level::Low });
        
        // Read the values from all signal pins
        let readings = [
            sig_pin_1.is_high(),
            sig_pin_2.is_high(),
            sig_pin_3.is_high(),
            sig_pin_4.is_high(),
        ];

        // Process each signal pin
        for (pin_idx, &reading) in readings.iter().enumerate() {
            let pin_number = (pin_idx + 1) as u8;
            
            if let Some(square_idx) = get_square_index(pin_number, channel) {
                // Update square
                if square_states[square_idx as usize].update(reading) {
                    let new_state = square_states[square_idx as usize].confirmed_state;
                    let square_name = SQUARE_NAMES[square_idx as usize];
                    
                    if new_state == true { 
                        // Piece was lifted (sensor now reads true)
                        if let Some(piece) = chess_board.get_piece_at(square_idx) {
                            let result = move_tracker.piece_lifted(square_idx, piece);
                            chess_board.remove_piece(square_idx);

                            match result {
                                MoveResult::PieceLifted(sq, p) => {
                                    println!("Turn: {:?} | Piece lifted from {}: {:?}", 
                                             move_tracker.get_current_turn(), square_name, p);
                                    display.clear(&mut delay).unwrap();
                                    display.reset(&mut delay).unwrap();
                                    display.write_str(p.to_str(), &mut delay).unwrap();
                                    let mut data: heapless::Vec<u8, 256> = heapless::Vec::new();
                                    data.extend_from_slice(p.to_str().as_bytes())
                                        .expect("Data too large for BLE buffer"); 


                                },
                                MoveResult::PieceCaptured(sq, p) => {
                                    println!("Turn: {:?} | Opponent piece captured at {}: {:?}", 
                                             move_tracker.get_current_turn(), square_name, p);
                                    println!("Now place your piece on {}", square_name);
                                },
                                MoveResult::WrongTurn => {
                                    println!("Not your turn! Current turn: {:?}", move_tracker.get_current_turn());
                                    chess_board.place_piece(square_idx, piece); // Put piece back
                                },
                                _ => {}
                            }
                        }
                    } else { 
                        // Piece was placed (sensor now reads false)
                        let result = move_tracker.piece_placed(square_idx);
                        
                        match result {
                            MoveResult::MoveCompleted(from_sq, to_sq, piece, captured) => {
                                chess_board.execute_move(from_sq, to_sq, piece, captured);
                                let from_name = SQUARE_NAMES[from_sq as usize];
                                let to_name = SQUARE_NAMES[to_sq as usize];
                                
                                if let Some(cap_piece) = captured {
                                    println!("Move completed: {:?} {} -> {} (captured {:?})",
                                             piece, from_name, to_name, cap_piece);
                                    let mut _move: String<16> = String::new();
                                    _move.push_str(piece.short_to_str()).unwrap();
                                    _move.push_str(from_name).unwrap();
                                    _move.push_str("x").unwrap();
                                    _move.push_str(to_name).unwrap();
                                    display.set_cursor_xy((0,1), &mut delay).unwrap();
                                    display.write_str(&_move, &mut delay).unwrap();
                                } else {
                                    println!("Move completed: {:?} {} -> {}", piece, from_name, to_name);
                                    let mut _move: String<16> = String::new();
                                    _move.push_str(from_name).unwrap();
                                    _move.push_str("->").unwrap();
                                    _move.push_str(to_name).unwrap();
                                    display.set_cursor_xy((0,1), &mut delay).unwrap();
                                    display.write_str(&_move, &mut delay).unwrap();
                                }
                                println!("Turn changed to: {:?}", move_tracker.get_current_turn());
                            },
                            MoveResult::PawnPromoted(from_sq, to_sq) => {
                                let from_name = SQUARE_NAMES[from_sq as usize];
                                let to_name = SQUARE_NAMES[to_sq as usize];
                                println!("Pawn promoted! {} -> {}", from_name, to_name);
                                println!("Please remove the pawn and place a Queen on {}", to_name);
                                let mut _move: String<16> = String::new();
                                _move.push_str(from_name).unwrap();
                                _move.push_str("->").unwrap();
                                _move.push_str(to_name).unwrap();
                                _move.push_str("=Q").unwrap();
                                display.set_cursor_xy((0,1), &mut delay).unwrap();
                                display.write_str(&_move, &mut delay).unwrap();
                            },
                            MoveResult::PawnPromotedWithCapture(from_sq, to_sq, captured) => {
                                let from_name = SQUARE_NAMES[from_sq as usize];
                                let to_name = SQUARE_NAMES[to_sq as usize];
                                println!("Pawn promoted with capture! {} -> {} (captured {:?})", 
                                         from_name, to_name, captured);
                                println!("Please remove the pawn and place a Queen on {}", to_name);
                                let mut _move: String<16> = String::new();
                                _move.push_str(from_name).unwrap();
                                _move.push_str("x").unwrap();
                                _move.push_str(to_name).unwrap();
                                _move.push_str("=Q").unwrap();
                                display.set_cursor_xy((0,1), &mut delay).unwrap();
                                display.write_str(&_move, &mut delay).unwrap();
                            },
                            MoveResult::PromotionCompleted(from_sq, to_sq, queen, captured) => {
                                chess_board.execute_promotion(from_sq, to_sq, queen, captured);
                                let from_name = SQUARE_NAMES[from_sq as usize];
                                let to_name = SQUARE_NAMES[to_sq as usize];
                                
                                if let Some(cap_piece) = captured {
                                    println!("Promotion completed: Pawn {} -> Queen {} (captured {:?})",
                                             from_name, to_name, cap_piece);
                                } else {
                                    println!("Promotion completed: Pawn {} -> Queen {}", from_name, to_name);
                                }
                                println!("Turn changed to: {:?}", move_tracker.get_current_turn());
                            },
                            MoveResult::InvalidAction => {
                                println!("Invalid move action at {}", square_name);
                            },
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    
    // Handle timeout for capture detection
    if let Some(timeout_result) = move_tracker.update_timeout() {
        match timeout_result {
            MoveResult::TimeoutReached => {
                println!("No capture detected - waiting for piece placement");
            },
            _ => {}
        }
    }
    
    Timer::after(Duration::from_millis(20)).await;
}
}

#[embassy_executor::task]
async fn ble_task(esp_wifi_ctrl: esp_wifi::EspWifiController<'static>, mut bluetooth: esp_hal::peripherals::BT) {
    use bleps::{
        Ble, HciConnector,
        ad_structure::{AdStructure, BR_EDR_NOT_SUPPORTED, LE_GENERAL_DISCOVERABLE, create_advertising_data},
        attribute_server::{AttributeServer, WorkResult},
        gatt,
    };
    use esp_wifi::ble::controller::BleConnector;

    let now = || time::Instant::now().duration_since_epoch().as_millis();

    loop {
        println!("Starting BLE connection...");

        let cmd_receiver = BLE_COMMAND_CHANNEL.receiver();
        let event_sender = BLE_EVENT_CHANNEL.sender();

        // Subject to change, debugging purposes
        let characteristic_data = RefCell::new({
        let mut data: heapless::Vec<u8, 256> = heapless::Vec::new();
        data.extend_from_slice(b"Initial data").unwrap();
        data
    });

        let connector = BleConnector::new(&esp_wifi_ctrl,&mut bluetooth);
        let hci = HciConnector::new(connector, now);
        let mut ble = Ble::new(&hci);

        println!("{:?}", ble.init());
        println!("{:?}", ble.cmd_set_le_advertising_parameters());
        println!(
            "{:?}",
            ble.cmd_set_le_advertising_data(
                create_advertising_data(&[
                    AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
                    AdStructure::ServiceUuids16(&[Uuid::Uuid16(0x1809)]),
                    AdStructure::CompleteLocalName(esp_hal::chip!()),
                ])
                .unwrap()
            )
        );
        println!("{:?}", ble.cmd_set_le_advertise_enable(true));

        println!("BLE advertising started - ready for connections");

        let mut rf = |_offset: usize, data: &mut [u8]| {
            if let Ok(char_data) = characteristic_data.try_borrow() {
                let len = char_data.len().min(data.len());
                data[..len].copy_from_slice(&char_data[..len]);
                len
            } else {
                // Fallback if RefCell is borrowed
                let fallback = b"Busy";
                let len = fallback.len().min(data.len());
                data[..len].copy_from_slice(&fallback[..len]);
                len
            }
        };
        
        let mut wf = |offset: usize, data: &[u8]| {
            println!("BLE RECEIVED: {} {:?}", offset, data);

            // Send received data to main task
            let mut vec_data = heapless::Vec::new();
            vec_data.extend_from_slice(data).ok();
            event_sender.try_send(BleEvent::DataReceived(vec_data)).ok();

            match core::str::from_utf8(data) {
                Ok(message) => {
                    println!("BLE message: {}", message);
                },
                Err(e) => {
                    println!("Error decoding BLE message: {:?}", e);
                }
            }
        };

        let mut wf2 = |offset: usize, data: &[u8]| {
            println!("RECEIVED: {} {:?}", offset, data);
            let mut vec_data = heapless::Vec::new();
            vec_data.extend_from_slice(data).ok();
            event_sender.try_send(BleEvent::CharacteristicWritten(1, vec_data)).ok();
        };

        let mut rf3 = |_offset: usize, data: &mut [u8]| {
            // This could be another piece of dynamic data
            data[..5].copy_from_slice(&b"Hola!"[..]);
            5
        };

       let mut wf3 = |offset: usize, data: &[u8]| {
            println!("RECEIVED: Offset {}, data {:?}", offset, data);
            let mut vec_data = heapless::Vec::new();
            vec_data.extend_from_slice(data).ok();
            event_sender.try_send(BleEvent::CharacteristicWritten(2, vec_data)).ok();
        };

        gatt!([service {
            uuid: "937312e0-2354-11eb-9f10-fbc30a62cf38",
            characteristics: [
                characteristic {
                    uuid: "937312e0-2354-11eb-9f10-fbc30a62cf38",
                    read: rf,
                    write: wf,
                },
                characteristic {
                    uuid: "957312e0-2354-11eb-9f10-fbc30a62cf38",
                    write: wf2,
                },
                characteristic {
                    name: "my_characteristic",
                    uuid: "987312e0-2354-11eb-9f10-fbc30a62cf38",
                    notify: true,
                    read: rf3,
                    write: wf3,
                },
            ],
        },]);

        let mut rng = bleps::no_rng::NoRng;
        let mut srv = AttributeServer::new(&mut ble, &mut gatt_attributes, &mut rng);

        event_sender.try_send(BleEvent::ClientConnected).ok();

        // Handle BLE connections and communication
        loop {
            if let Ok(command) = cmd_receiver.try_receive() {
                match command {
                    BleCommand::SendData(data) => {
                        if let Ok(mut char_data) = characteristic_data.try_borrow_mut() {
                            char_data.clear();
                            char_data.extend_from_slice(&data).ok();
                            //println!("Updated characteristic data: {:?}", char_data);
                        }
                    }
                    BleCommand::UpdateCharacteristic(id, data) => {
                        //println!("Update characteristic {} with data: {:?}", id, data);
                        // Handle different characteristics based on ID
                        if id == 0 {
                            if let Ok(mut char_data) = characteristic_data.try_borrow_mut() {
                                char_data.clear();
                                char_data.extend_from_slice(&data).ok();
                            }
                        }
                    }
                }
            }
            match srv.do_work() {
                Ok(res) => {
                    if let WorkResult::GotDisconnected = res {
                        println!("BLE client disconnected - restarting advertising");
                        break; // Break inner loop to restart advertising
                    }
                }
                Err(err) => {
                    println!("BLE error: {:?}", err);
                    break; // Break inner loop to restart
                }
            }
            //Timer::after(Duration::from_millis(1000)).await;
        }
    }
}

pub fn send_ble_command(command: BleCommand) {
    BLE_COMMAND_CHANNEL.sender().try_send(command).ok();
}

pub async fn wait_for_ble_event() -> BleEvent {
    BLE_EVENT_CHANNEL.receiver().receive().await
}

pub fn try_get_ble_event() -> Option<BleEvent> {
    BLE_EVENT_CHANNEL.receiver().try_receive().ok()
}
